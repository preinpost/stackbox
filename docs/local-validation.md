# 이전 다중 VM OpenStack 실배포 검증 — 2026-10-20

이 문서는 이전 다중 VM 구성의 역사적 검증 기록이다. 현재 단일 VM 구조는 [single-vm.md](single-vm.md)를 참고한다.

macOS Apple Silicon에서 실제 microsandbox VM 5개로 Keystone·Glance를 배포하고,
호스트의 공개 API와 VM 재시작 후 데이터 유지를 확인했다. fake backend 테스트와는 별개다.
배포 중 발견한 오류를 수정하고 해당 단계부터 재개해 완료했다.

## 환경

| 항목 | 값 |
| --- | --- |
| microsandbox | 0.7.8, 로컬 macOS 가상화 |
| Stackbox | `local-e2e`, compact, VM 5개, 총 6144 MiB 할당 |
| 게스트 | 갱신한 `stackbox-golden:rocky10`, ARM64 |
| Kolla Ansible | stable/2026.2, `23.0.0.0rc3.dev5` |
| 컨테이너 태그 | `2026.2-rocky-10-aarch64` |
| Docker | 29.9.1 |
| OpenStack CLI | 10.3.0 |
| 공개 주소 | Keystone `http://localhost:15000/v3`, Glance `http://localhost:15001` |

## 확인 결과

| 검사 | 결과 |
| --- | --- |
| VM 생성·시스템 부팅·전체 노드 간 VXLAN 통신 | 통과 |
| Kolla install / configure / bootstrap / prechecks / deploy / post-deploy / verify | 완료 |
| Stackbox manager 최종 상태 | `ready`, phase `verify`, error 없음 |
| HAProxy / MariaDB / RabbitMQ / Memcached | health check 통과 |
| Keystone API / Fernet / SSH 보조 컨테이너 | health check 통과 |
| Glance API | health check 통과 |
| VM 내부 OpenStack CLI | 토큰 발급, 이미지 업로드·다운로드 내용 비교·삭제 통과 |
| 공개 Keystone API | 비밀번호 인증, project-scoped 토큰, 서비스 카탈로그, 프로젝트 목록 조회 통과 |
| 공개 Glance API | 인증 없는 요청 401, 1 MiB 업로드, active 상태, 다운로드 SHA-256 일치, 삭제 후 404 통과 |
| VM 5개 재시작 | 서비스 자동 복구, `/run` 공유 상태 유지 |
| 재시작 후 영속성 | 새 토큰 발급, 재시작 전 이미지 다운로드 SHA-256 일치, 삭제 통과 |
| Ansible 로그 | 정상 출력에 서비스 비밀값 없음; 실패 상세 0600; `no_log` 내용 비공개 |
| 코드 검사 | Rust 테스트 24개, fmt, Clippy 통과; Linux ARM64 agent 빌드 통과 |

재시작 후 대상 VM의 컨테이너 16개가 실행 중이며, health check가 있는 서비스는 모두
`healthy`인 것을 확인했다. 설치 검증에 사용한 이미지는 테스트 종료 시 삭제한다.

## 이번 실배포에서 수정한 문제

1. **golden 이미지 태그 갱신 누락**: `docker save | msb load`만 사용하면 예전의 짧은
   `stackbox-golden:rocky10` 참조가 남을 수 있었다. 로컬 Task와 원격 준비 경로에
   `msb load --tag stackbox-golden:rocky10`을 적용하고 새 VM의 `selinux` Python 모듈을 확인했다.
2. **`/run` 마운트 전파**: Kolla toolbox가 요구하는 공유 마운트가 없어 컨테이너 시작이
   실패했다. prepare에서 공유 상태를 적용하고 Docker의 `ExecStartPre`에도 등록했다.
3. **불필요한 `ip_vs` 로드**: Kolla 2026.2가 keepalived 비활성화 상태에서도 모듈을
   로드했다. 설치 시 해당 작업에만 `enable_keepalived` 조건을 추가한다.
4. **DB 주소 오류**: ProxySQL을 끈 구성에서 DB 주소가 LB를 가리켰지만 MariaDB용
   HAProxy listener는 생성되지 않았다. 단일 infra 노드의 주소를 `database_address`로 지정한다.
5. **서비스 비밀값이 포함된 Ansible 출력**: 기본 callback의 결과 payload를 전달하지 않는
   Stackbox callback을 추가했다. root 전용 상세 파일과 `no_log` 동작을 실제 실패 작업으로 검증했다.

Glance 검증에는 다운로드 내용 비교와 고유 이미지 이름·실패 시 정리를 추가했다.
공개 API 검증은 [verify-openstack.py](../scripts/verify-openstack.py)로 반복할 수 있다.
`--restart-local-vms`를 지정하면 이미지 업로드 후 해당 로컬 Stackbox만 재시작하고 영속성을 검사한다.
재현 절차는 [README의 코드와 검증](../README.md#코드와-검증)을 참고한다.

## 범위

이번 결과는 단일 로컬 호스트의 compact 구성과 Keystone·Glance에 대한 것이다.
오류 수정 후 단계 재개를 포함한 실제 배포를 검증했으며, 최종 코드를 완전히 새 Stackbox에서
다시 처음부터 한 번에 실행하는 반복 배포는 별도로 수행하지 않았다.
Nova·Cinder와 여러 타겟 호스트에 컴포넌트를 나누는 구성은 아직 구현·검증 범위 밖이다.

## 선택 서비스 (2026-10-21)

| Stackbox | 구성 | 결과 |
|---|---|---|
| `p1` | compact + Placement·Horizon, VM 6개, 7424 MiB | 배포·VM 내부 verify·공개 포트 검증·6대 재시작 후 컨테이너 20개 healthy |
| `n1` | compact + Placement·Horizon·Neutron, VM 7개, 10240 MiB | 아래 참고 |

`n1`은 새 Stackbox에서 생성 후 배포했고 OVS·L3 agent 문제를 고치며 `prepare`부터 재개했다.
최종 코드로 VM 내부 verify(agent 4개, 임시 router 게이트웨이와 DHCP 포트 ping), 공개 포트의
Placement·Horizon 로그인·Neutron 외부망/agent 조회, VM 7대 재시작 후 컨테이너 31개 healthy,
재시작 뒤 VM 내부 verify 재실행까지 통과했다. 재시작 직후 agent는 RabbitMQ 복구를 기다리느라
약 1분 늦게 alive가 된다.
원격 Linux KVM에서의 전체 OpenStack 배포 결과로 해석하지 않는다.
