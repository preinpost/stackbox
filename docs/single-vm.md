# Farm당 VM 1대

새 Farm은 `<id>-all-in-one` VM 하나에 Ansible 실행기와 선택한 모든 OpenStack 서비스를
설치한다. 별도 deployer·LB·infra·control·compute VM과 Farm 관리 VXLAN은 만들지 않는다.
배포 inventory는 Kolla가 제공하는 all-in-one이며 Ansible 연결은 local이다.

## 자원과 네트워크

- 기본 2 vCPU, 3072 MiB에 선택한 서비스의 메모리를 더한다.
- Placement·Horizon·Neutron을 모두 고르면 6144 MiB, Nova까지 고르면 8192 MiB다.
- root disk 20 GiB와 Docker·containerd용 owned ext4 disk 각 32 GiB를 사용한다.
- 서비스는 고정 서비스 주소(`farm0`의 `10.254.0.2`)에 바인딩하고 호스트의 기존
  `15000 + 10n + offset` 포트로 공개한다. eth0으로 들어온 연결은 nftables DNAT로 전달한다.
  msb가 시작할 때마다 eth0 주소를 다시 고르므로, 재시작·스냅샷 복원 후에도 같은 주소와
  hostname(`farm-aio`)을 유지하기 위해서다. 자세한 내용은 [snapshots.md](snapshots.md)를 참고한다.
  이 기능 이전에 만든 farm은 계속 eth0 주소로 배포한다.
- HAProxy·keepalived·ProxySQL은 사용하지 않는다. MariaDB 버퍼풀 512 MiB,
  API/RPC worker 1개, Memcached 64 MiB로 공유 메모리 사용을 제한한다.
- `farm-identity.service`가 hostname과 서비스 주소를 적용하고, `farm-hostname.service`가
  호스트명을 그 주소(이전 farm은 eth0 주소)로 복원한 뒤 Docker를 시작한다.
  `farm-external.service`는 Neutron 선택 시 외부 veth와 nftables NAT만 구성한다.
- msb 커널 제약 때문에 Neutron의 userspace OVS와 기존 커널 호환 패치는 유지한다.

## 호환성

기존 역할 enum과 상태 파일은 읽을 수 있으며 기존 VM을 자동 삭제·변환하지 않는다.
기존 다중 VM Farm의 목록·조회·인증정보 조회·삭제는 유지하되 재배포는 `409`로 거절한다.
기존 데이터를 보관한 뒤 새 Farm을 만들며, `layout: multi` 선택지는 제공하지 않는다.

구형 원격 실행기는 고정 digest로 기존 Farm을 관측·삭제하는 데 사용한다. 새 Farm 생성은
`supports_single_vm: true`인 새 실행기에서만 허용하므로 Coordinator와 실행기를 함께 갱신한다.
API의 `plan.nodes`는 새 계획에서 한 개이며, Role의 `all-in-one` 값이 추가됐다.
SDK inventory에 live IP가 없으면 `nodes[].ip`는 null일 수 있고 배포 시 eth0 주소를 직접 확인한다.

## 검증

단위/API/SSH 테스트와 포탈 테스트에 단일 VM 계획·기존 Farm 보존·배포 거절·실행기 호환성·
호스트명 재부팅 복구를 포함한다. 실제 설치 검증에는 `scripts/verify-openstack.py`를 사용하며,
`--restart-local-vms`는 단일 로컬 VM을 재시작하고 이미지 영속성·선택 서비스·컨테이너 상태를 검사한다.
이전 다중 VM의 실배포 결과는 [local-validation.md](local-validation.md)의 역사적 기록이다.

### 2026-10-21 로컬 실배포 결과

macOS aarch64/HVF의 microsandbox에서 새 golden image로 아래 두 Farm을 실제 생성했다.
Kolla-Ansible `23.0.0.0rc3.dev5`, Kolla `2026.2-rocky-10-aarch64` 이미지를 사용했다.
두 구성 모두 prepare부터 verify까지 완료했으며 VM 재부팅 뒤 공개 API와 내부 verify를 다시 통과했다.

| 구성 | VM | 예약 메모리 | 컨테이너 | 재부팅 후 메모리 사용 / 가용 |
| --- | --- | --- | --- | --- |
| Keystone + Glance | 1 | 3072 MiB | 9 | 832 / 2093 MiB |
| 위 서비스 + Placement + Horizon + Neutron | 1 | 6144 MiB | 20 | 3387 / 2550 MiB |

메모리는 검증 직후 `free -m`의 단일 관측값이며 부하 테스트나 최대 사용량 보장은 아니다.
두 VM 모두 swap 없이 동작했고 수집한 커널 로그에서 OOM 종료가 없었다.

- Keystone: 비밀번호 인증, scoped token, public catalog, 인증된 프로젝트 조회.
- Glance: 비인증 요청 401, 1 MiB 이미지 업로드·활성화·다운로드 SHA-256 일치·삭제 후 404.
  이미지를 업로드한 채 VM을 재부팅하고 새 토큰과 기존 이미지의 동일한 체크섬을 확인했다.
- Placement: 인증된 resource provider 조회. Horizon: 공개 포트의 CSRF 기반 로그인.
- Neutron: 외부 public network, 네 에이전트의 alive 상태, 임시 라우터 gateway와 DHCP port ping.
  재부팅 뒤 새 임시 네트워크로 통신 검사를 반복하고 검증 리소스를 삭제했다.
- 재부팅 뒤 9개/20개 컨테이너가 모두 실행 중이고 정의된 health check가 모두 healthy였다.
  자기 호스트명 복구와 `/run` shared mount도 확인했다.
- Rust 테스트 28개, Clippy `--all-targets -- -D warnings`, 포털 테스트 21개와 production build 통과.
  Linux aarch64 실행기도 재빌드하여 protocol 1의 Hello가 `supports_single_vm: true`임을 확인했다.
- 실제 포털에서 1 VM 계획, 선택 서비스의 메모리 합산, 배포 완료 화면의 다섯 엔드포인트,
  기존 7 VM Farm의 배포 비활성화와 조회·삭제 UI를 확인했다. 390px/1280px에서 가로 넘침이 없었다.

테스트용 `single-e2e`와 `single-base-e2e`는 검증 뒤 삭제했다. 기존 `n1`의 7개 VM은 보존했다.
원격 Linux/KVM 호스트의 전체 배포는 이번 검증 범위에 포함하지 않는다.
Nova는 이후 `nova-e2e`(8192 MiB)에서 확인했다. qemu TCG로 Cirros aarch64 인스턴스를 부팅하고, 라우터 namespace에서 고정 IP ping이 통과했다. 공개 Nova API의 서버 목록 조회도 통과했다.

2026-10-21에는 nested virtualization을 넣은 msb 0.7.8(Apple M5 Pro, macOS 26.6.2)로 포털 API에서
`nova-kvm`(Nova·Horizon, 8192 MiB)을 생성·배포했다. 게스트 `/dev/kvm`을 감지해 `virt_type=kvm`,
`cpu_mode=host-passthrough`로 구성됐고 verify의 Cirros 부팅·ping이 통과했다. 별도 인스턴스는
생성 5초 만에 ACTIVE가 됐고 libvirt 도메인이 `type='kvm'`임을 확인했다. 검증 뒤 farm은 삭제했다.
