# 포탈 backend 연동

이 저장소의 웹포탈은 [web/README.md](../web/README.md)를 참고하세요.
포탈은 Horizon 카드에 admin 로그인(사용자, 비밀번호, 도메인)을 표시하기 위해
`OS_USERNAME`, `OS_PASSWORD`, `OS_USER_DOMAIN_NAME`, `OS_PROJECT_NAME`,
`OS_PROJECT_DOMAIN_NAME`만 브라우저로 전달합니다. 그 외 필드는 제거합니다.
원격 호스트 등록·SSH 점검·환경 준비와 `/host-jobs` 계약은
[원격 호스트 운영](remote-hosts.md)에 정리했습니다. 모든 Farm은 등록된 호스트에서 실행하므로
`POST /farms`에는 `host_id`가 필요합니다. coordinator가 도는 머신은 `POST /local-host`로 로컬 호스트로 등록한 뒤 다른 호스트처럼
`POST /hosts/{id}/check`, `POST /hosts/{id}/prepare`로 점검·준비합니다.

API prefix는 `/api/v1`입니다. token이 설정된 경우 모든 경로에
`Authorization: Bearer ...`를 전달합니다. 성공·오류 응답은 JSON이며, SSE는 예외입니다.
작업 접수는 `202 Accepted`, 잘못된 입력은 `400`/`422`, 존재하지 않는 farm/job은 `404`,
중복 작업·메모리 부족·잘못된 상태 전이는 `409`입니다. 내부 오류는 `500`이며 상세는 데몬 로그에 남습니다.

## 생성과 배포

1. `GET /hosts`, `GET /catalog`로 실행할 호스트의 준비 상태와 지원 범위를 확인합니다.
2. `POST /plans`에 `{"components":["horizon"]}`처럼 추가할 서비스를 전달합니다.
   필수 컴포넌트(Keystone·Glance)와 선택 서비스의 `depends_on`은 자동으로 포함되며, 응답의
   `components`는 catalog 순서로 정리된 최종 목록입니다. 모르는 컴포넌트나 `available: false`인
   컴포넌트는 `400`으로 거절합니다. 새 계획의 `nodes`는 항상 `role: "all-in-one"` 한 개입니다.
   Placement·Horizon·Neutron은 같은 VM에 설치하며 선택한 서비스만큼 메모리가 증가합니다. 이 단계는 포트·메모리를 예약하지 않는 순수 계산입니다.
3. `POST /farms`에 `id`, `components`, `external_host`, `owner`, `expires_at`을 전달합니다.
   `host_id`는 `ready`인 호스트여야 하며, 없으면 `400`, 모르는 호스트는 `404`입니다.
   원격 호스트에는 타겟의 접근 가능한 IPv4/DNS `external_host`를 지정하세요.
   필수 값은 `id`와 `host_id`입니다. `expires_at`은 UTC Unix seconds이며 현재 metadata로만 저장합니다.
   `id`는 1~48자의 소문자·숫자·하이픈이며 양끝에 하이픈을 쓸 수 없습니다.
4. 응답의 `id` 또는 `Location`으로 `GET /jobs/{id}`를 polling합니다.
   job 상태는 `queued → running → succeeded/failed`입니다. 데몬 강제 종료 시 `interrupted`가 될 수 있습니다.
   실행 중인 배포 job은 예외입니다. 배포는 호스트에서 계속되고, 다시 시작한 데몬이 같은 job에 다시 붙어
   `running`에서 이어 갑니다. 호스트에 다시 연결하지 못할 때만 `interrupted`가 됩니다.
5. create 성공 후 `POST /farms/{id}/deploy`에 `{}`를 전달합니다.
   배포 job이 성공하면 farm 상태가 `ready`가 됩니다. 기존 다중 VM Farm의 재배포는 `409`로 거절하며 자동 변환하지 않습니다.
6. 실패하면 job의 `phase`와 `error`를 표시합니다. 재개 요청 예:
   `POST /farms/{id}/deploy`, body `{"from":"prechecks"}`.
7. 삭제는 `DELETE /farms/{id}`로 요청하고 동일하게 job 완료를 기다립니다.
   삭제 성공 후에도 개별 farm 조회에는 `deleted` tombstone이 남지만 목록에서 제외됩니다.
   호스트가 없어졌거나 접속되지 않으면 `DELETE /farms/{id}?forget=true`로 호스트에 묻지 않고
   기록만 지웁니다. 호스트에 남은 VM은 그대로이며, 호스트를 purge할 때 정리됩니다.

### 배포된 farm에 compute 추가

배포가 끝난 다중 VM farm에는 `POST /farms/{id}/computes`로 compute VM을 한 대 더 붙입니다.
body의 `host_id`는 새 VM을 실행할 호스트이며, 생략하면 farm의 `host_id`를 씁니다. 응답은 `202`와
`expand` job이고, `job.target_host`와 `job.target_node`가 대상 호스트와 `compute-N`을 가리킵니다.
같은 호스트는 링크 포트만 추가하고, 원격 호스트는 새 share와 uplink를 만듭니다. 이미 farm에 참여한
원격 호스트는 기존 uplink를 재사용합니다. farm이 전부 한 호스트에 있었는데 처음 원격 compute를 붙이면
control VM의 호스트도 relay에 붙습니다.

확장은 `passwords.yml`과 `globals.yml`을 다시 쓰지 않고 Kolla inventory만 갱신한 뒤 새 compute에만
`bootstrap-servers`와 `deploy --limit farm-compute-N`을 실행합니다. farm의 `plan`은 Kolla 성공 뒤에만
새 layout으로 바뀌므로, 실패한 `expand` job은 같은 farm에 `POST /farms/{id}/computes`를 다시 보내
이어서 재시도할 수 있습니다. resume 요청은 마지막으로 실패한 `expand` job의 `target_host`와
`target_node`를 그대로 씁니다.

body에 `from_snapshot`(compute 템플릿 스냅샷 ID)을 넣으면 새 VM을 그 디스크로 부팅하고, identity를
지운 뒤 `reconfigure --limit`과 `nova-manage cell_v2 discover_hosts`만 실행합니다. job의 `snapshot_id`가
그 템플릿이며, 실행 중에는 템플릿을 지울 수 없습니다. 템플릿은 `POST /farms/{id}/snapshots`에
`{"id":"...","node":"compute-1"}`로 만듭니다(배포된 farm, farm의 primary 호스트에 있는 compute만).
스냅샷의 `template`이 `"compute"`이고 `release`가 OpenStack 릴리스입니다(이전 버전의
`compute_template: true` 기록은 읽을 때 `template: "compute"`로 바뀝니다). 템플릿이 아니거나 farm과
릴리스·서비스·아키텍처가 다르거나, 대상이 로컬 호스트의 control VM 호스트가 아니면 `409`입니다. 실패한
확장은 처음과 같은 `from_snapshot`으로만 재시도할 수 있습니다.

`{"id":"...","node":"control"}`은 control VM만 **control 템플릿**(`template: "control"`)으로 얼립니다.
compute를 먼저 멈추고 control을 먼저 켜며, 스냅샷의 `plan`은 control VM 하나(`computes: 0`)입니다.
`POST /farms`에 그 `snapshot_id`를 넣으면 compute 없는 다중 VM farm이 `restore` job으로 만들어지고,
job 로그에 원본 compute 등록(nova-compute 서비스, placement, Neutron agent, cell host mapping)을 지우는
단계가 이어집니다. 이 farm은 `ready`가 되면 위와 같이 compute를 추가할 수 있고, `deploy`와 farm 전체
얼리기는 `409`입니다. compute 템플릿으로 `POST /farms`를 보내면 `409`입니다.

`ready` 상태의 다중 VM farm이고 compute가 3대 미만일 때만 허용합니다(control 템플릿으로 만든 compute
0대 farm 포함). 스냅샷에서 복원한 다중 VM farm도
대상입니다. all-in-one farm, 다른 작업이 진행 중인 farm은 `409`입니다. 대상 호스트의 메모리 예산을 넘거나 원격
호스트인데 relay가 없어도 `409`입니다. 기존 인스턴스는 새 compute로 자동 이동하지 않습니다.

farm 상태는 `creating`, `created`, `deploying`, `settling`, `ready`, `failed`, `broken`, `deleting`, `deleted`입니다.
배포 중에는 `phase`와 함께 `step`이 채워집니다. `step`은 그 단계 안에서 지금 실행 중인 작업이며,
설치·검증 등 farm이 직접 실행하는 구간은 단계 이름을, Kolla-Ansible 구간은 현재 Ansible task 이름을
담습니다. 단계가 바뀌면 비워지고, 마지막 값은 job이 끝난 뒤에도 남습니다. 포탈은 이 값을 배포 단계
목록의 현재 단계 아래에 표시합니다.
`creating`/`deploying`은 job이 큐에서 대기하는 시간도 포함하므로 job 상태를 같이 표시하세요.
VM 관측 결과는 시작 시와 30초 주기로 갱신되며, 실행 중인 job의 farm은 reconcile에서 건너뜁니다.
`ready`는 마지막 배포가 성공하고 예상 VM이 실행 중이라는 뜻이며 OpenStack API의 상시 health check는 아닙니다.

## 스냅샷

`POST /farms/{id}/snapshots`에 `{"id":"dev-clean"}`를 전달하면 `snapshot` job이 VM을 정지해
디스크를 저장하고 다시 시작합니다. farm 상태는 바뀌지 않으며 진행 중에는 `active_job`이 설정됩니다.
job의 `steps`에는 시작한 순서대로 번호 붙은 단계가 쌓입니다(`step`, `index`/`total`, `started_at`,
끝난 단계는 `finished_at`, 실패한 단계는 `failed: true`). 마지막 항목에 `finished_at`이 없으면 지금
실행 중인 단계입니다. farm의 `step`에도 `2/5 capture ...` 형태로 같은 값이 들어갑니다.
디스크 캡처는 바이트 진행률을 알려주지 않으므로 단계 단위로만 표시합니다.
`POST /farms`에 `snapshot_id`를 지정하면 `restore` job이 스냅샷에서 VM을 만들고, 배포된 스냅샷이면
성공 시 바로 `ready`가 됩니다. 이때 `components`와 `host_id`는 생략합니다. 두 job 모두 `snapshot_id`를 포함합니다.

내보내기(`POST /snapshots/{id}/export`)와 가져오기(`POST /snapshots/import`)는 `202`와 스냅샷 레코드를
반환합니다. 스냅샷이 `exporting`/`importing` 상태를 벗어날 때까지 `GET /snapshots/{id}`를 polling하고,
`error`와 `archive.path`를 표시하세요. 경로는 farmd 호스트의 절대 경로이며 상대 경로는 `400`입니다.
`arch`는 스냅샷을 얼린 CPU 아키텍처입니다. 다른 아키텍처의 스냅샷은 복원(`POST /farms`)과
가져오기(`POST /snapshots/import`)에서 `409`로 거절합니다.
버킷 목록(`GET /buckets/{id}/objects`)의 각 객체 `metadata`는 내보내기가 아카이브 옆에 쓴 메타데이터
파일(`release`, `role`, `components`, `arch`, `sha256` 등)이고, 예전 아카이브는 `null`("메타데이터 없음")입니다.
버킷에서 가져온 아카이브는 메타데이터 파일의 SHA-256과 다르면 `failed`가 됩니다.
`POST /buckets/{id}/index`는 prefix의 `index.json`을 메타데이터 파일에서 다시 만듭니다.
`POST /farms/{id}/settle`은 배포를 다시 실행하지 않고 Keystone과 선택 서비스의 응답만 기다립니다.
farm 상태가 `settling`으로 바뀌고, 성공하면 `ready`, 실패하면 `created`로 돌아갑니다.
VM을 직접 재시작했거나 host가 재부팅된 뒤에 사용합니다.
자세한 동작은 [snapshots.md](snapshots.md)를 참고하세요.

## 로그

`GET /farms/{id}/logs?after=<event-id>&limit=1000`으로 최근 로그를 읽습니다.
`GET /farms/{id}/logs/stream`의 SSE는 `event: log`, 증가하는 `id`, JSON `LogEntry`를 전달합니다.
`job_id`로 여러 작업의 로그를 구분할 수 있습니다. `message`는 출력 chunk이므로 항상 한 줄은 아닙니다.

SSE 재연결 시 `Last-Event-ID` 또는 `after` query를 전달하면 이후 이벤트부터 받습니다.
재생 가능한 범위는 최근 1,000개 이벤트이며, 오래된 cursor에는 `event: gap`을 보낸 뒤
보관 중인 tail을 재생합니다. 15초마다 keepalive comment를 보냅니다.
`event: error`는 로그 읽기 실패를 의미합니다. 연결이 종료되면 재연결하세요.
서비스 종료 시 stream은 닫힙니다. `limit`은 JSON 로그 조회에만 적용됩니다.

## 셸

`GET /farms/{id}/shell?rows=<1-500>&cols=<1-1000>`을 WebSocket으로 열면 farm VM 안에서
`bash -l`이 root PTY로 실행됩니다. 터미널 바이트는 양방향 모두 binary frame으로 오가고,
text frame은 제어 메시지입니다. 클라이언트는 `{"type":"resize","rows":40,"cols":120}`을 보내고,
서버는 셸이 끝나면 `{"type":"exit"}`, 실패하면 `{"type":"error","message":"..."}`를 보낸 뒤 닫습니다.
연결이 끊기면 셸에 SIGHUP을 보내 그 안에서 실행 중인 작업도 함께 종료합니다.

다중 노드 farm은 `&node=<control|compute-N>`으로 셸을 열 VM을 고릅니다(없는 VM은 `404`).
생략하면 all-in-one VM, 다중 노드 farm에서는 control VM에 열립니다.

대상 VM이 실행 중이어야 하며(`409`), 배포 등 작업 중에도 열 수 있습니다. 원격 호스트 farm은
farm-agent가 SSH로 중계하므로 이 기능이 들어간 agent를 호스트에 다시 설치해야 합니다.
WebSocket에는 CORS가 적용되지 않으므로 farmd는 `Origin`이 없거나(포탈 프록시) 같은 호스트일 때만
연결을 받고, 포탈 프록시는 브라우저의 `Origin`이 포탈 자신일 때만 중계합니다.
셸은 VM의 root 권한이므로 외부에 공개된 포털이라면 API token과 포탈 사용자 권한을 반드시 설정하세요.

## 인증정보

`GET /farms/{id}/endpoints`는 **ready 상태일 때만** admin 인증정보를 반환합니다.
`datastores`에는 farm마다 함께 설치되는 MariaDB(`mysql://`), RabbitMQ(`amqp://`), RabbitMQ 관리 UI가
호스트 포트·계정·비밀번호·데이터베이스 목록과 함께 들어갑니다. 포탈은 이 중 필요한 필드만
브라우저로 전달합니다.
`services`는 서비스 ID별 public URL이며 `credentials`는 `OS_*` 환경 변수 mapping입니다.
새 관리 farm에서는 `OS_AUTH_URL`이 public Keystone URL, `OS_INTERFACE`가 `public`으로 반환됩니다.
비밀번호는 guest의 admin-openrc에서 매번 읽고 host 상태 파일에는 저장하지 않습니다.

응답에는 `Cache-Control: no-store`를 설정합니다. 로컬 테스트 포털은 Horizon 카드에 admin 로그인을 표시합니다.
외부에 공개된 포털이라면 이 표시를 끄세요. 로그에도 민감한 배포 정보가 있을 수 있으므로 API token을 설정하고
포탈 사용자의 접근 권한을 backend에서 확인하세요.

## 현재 제약

- 배포 가능한 서비스는 Keystone·Glance(필수)와 Placement·Horizon·Neutron(선택)입니다.
  Nova는 게스트에 nested `/dev/kvm`이 있으면 KVM, 없으면 qemu TCG로 동작합니다. Neutron은 OVS userspace datapath로 동작합니다. `endpoints`의 `services`에는
  farm 계획에 포함된 서비스의 public URL만 들어갑니다.
- Coordinator는 VM 생성과 배포를 각각 한 작업씩 실행합니다. 예약은 실행 호스트별이며, agent도 해당 호스트의 생성/배포 작업과 자원을 검사합니다.
- 원격 조회 실패 시 `observation_error`를 표시하고 마지막 `nodes`와 메모리·포트 예약을 유지합니다. `host_id`, `remote_generation`, `agent_sha256`은 생성 이후 유지됩니다.
- job 취소·요청 idempotency key·TTL 삭제·로그 회전·farm별 사용자 권한은 아직 없습니다.
  POST 응답을 잃어버렸다면 목록/상태를 먼저 조회하세요. 동일 ID의 생성 재요청은 충돌로 처리됩니다.
- `GET /host`의 `hypervisor_available`은 Linux에서는 `/dev/kvm` 접근 여부,
  macOS에서는 플랫폼 지원 여부입니다. 실제 VM 부팅 성공을 보장하는 검사는 아닙니다.
- 기존 CLI로 만든 farm은 `plan`과 `exposure`가 `null`일 수 있습니다. 기존 역할 값은 읽기·삭제 호환성을 위해 유지합니다. VM 메모리는 별도로 관측해 예약에 포함합니다.
- state 디렉터리를 별도로 지정해 운영 데몬 여러 개를 같은 msb 호스트에 실행하지 마세요.
