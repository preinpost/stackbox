# farm

Farm마다 microsandbox VM을 만들고 Kolla로 OpenStack을 설치하는 Rust CLI와 REST API 데몬입니다.
기본은 VM **1대**(all-in-one)로, 선택한 모든 서비스와 Ansible 실행기가 같은 VM에서 동작합니다.
**control 1 + compute 1~3** 구성도 고를 수 있습니다. 이때 VM들은 호스트의 링크 스위치로 이어지고,
compute VM을 고립시켜 evacuate 같은 장애 대응을 시험할 수 있습니다([다중 VM farm](#다중-vm-farm)).
웹포탈 backend는 `farm serve`의 API를 호출해 farm 생성·배포·삭제 작업과 로그를 추적할 수 있습니다.

항상 **Keystone + Glance**를 배포하고, 선택 서비스로 **Placement**, **Horizon**, **Neutron**,
**Nova**를 추가할 수 있습니다. 선택한 서비스의 의존성은 자동으로 포함됩니다.
Nova는 microsandbox nested virtualization으로 게스트에 `/dev/kvm`이 있으면 KVM으로,
없으면 qemu TCG로 작은 인스턴스만 실행합니다.

## 시작

`msb`와 `libkrunfw`가 `~/.microsandbox`에 설치되어 있어야 합니다
(`MSB_PATH`, `MSB_LIBKRUNFW_PATH`로 지정 가능). macOS에서는 호스트에서 직접 실행하고,
Linux에서는 접근 가능한 `/dev/kvm`이 필요합니다.

```bash
task golden                     # 선택: golden image를 미리 빌드 (없으면 첫 farm이 빌드)
cargo build --release

./target/release/farm catalog --json
./target/release/farm plan --components horizon --json
./target/release/farm serve      # http://127.0.0.1:8080
```

포탈 없이 CLI만으로도 같은 관리 로직을 사용할 수 있습니다.
CLI는 작업이 끝날 때까지 기다립니다.

```bash
farm host local                 # 로컬 호스트로 등록하고 준비 (시험 VM 부팅)
farm host list
farm create demo --host-id local                      # 1 VM / 3 GiB (Keystone·Glance)
farm create web --host-id local --components horizon  # 선택 서비스만큼 메모리 증가
farm deploy demo
farm deploy demo --from prechecks
farm status demo --json
farm list --json
farm endpoints demo --json      # 배포 성공 후 명시적으로만 admin 인증정보 조회
farm logs demo
farm delete demo

farm snapshot create demo demo-clean     # VM을 얼려두기 (잠시 정지 후 재시작)
farm create demo2 --host-id local --snapshot demo-clean  # 다시 설치하지 않고 불러오기
farm snapshot export demo-clean -o ~/farm-snapshots/
farm snapshot import ~/farm-snapshots/demo-clean.msb
farm snapshot export demo-clean --bucket team-r2   # 등록한 S3 호환 버킷(AWS·R2·MinIO·RustFS)으로
farm settle demo                         # VM 재시작 뒤 배포 없이 서비스 응답만 확인
```

스냅샷의 동작과 제약은 [docs/snapshots.md](docs/snapshots.md)를 참고하세요.

데몬과 CLI는 같은 상태 디렉터리에 대해 배타 잠금을 사용합니다.
**데몬 실행 중에는 포탈/API로 작업하고, 직접 CLI를 사용하려면 데몬을 먼저 종료**합니다.
`catalog`, `plan`, `openapi`는 잠금 없이 실행됩니다.

## Golden image

모든 farm VM은 호스트의 golden image에서 시작합니다. golden image는 msb 디스크 스냅샷
`farm-golden-<os>:<레시피 해시>`입니다. Docker도 호스트에 미리 준비할 것도 없습니다.

- 호스트에 그 스냅샷이 없으면 첫 farm을 만들 때 msb가 빌드합니다. 배포판 공식 OCI 이미지
  (`rockylinux/rockylinux:10`)로 VM을 띄워 `guest/os/<os>.sh`를 실행하고, VM을 멈춘 뒤 디스크(root와
  Docker·containerd용 디스크 두 개)를 스냅샷으로 뜹니다. M1 Max에서 약 35초 걸렸습니다.
  `task golden`(`farm golden`)으로 미리 빌드하고, `farm golden --check`로 확인할 수 있습니다.
- `guest/os/<os>.sh`에는 배포판 패키지 설치만 둡니다. farm 전용 파일(`guest/farm/`의 identity·hostname·
  외부망 유닛)은 VM을 만들 때와 배포할 때 넣으므로, 이 파일을 바꿔도 golden image를 다시 빌드하지 않습니다.
- 스냅샷 이름의 해시는 기본 이미지, 설치 스크립트, 디스크 크기로 정해집니다. 셋 중 하나가 바뀌면 새 이름이
  되어 다음 farm이 새로 빌드합니다. 옛 스냅샷은 그 위에서 만든 farm과 스냅샷이 있는 동안 남아 있습니다.
- 새 배포판은 `guest/os/<id>.sh`와 `src/farm/golden.rs`의 `GuestOs` 항목 하나로 추가합니다. 스크립트는
  systemd(`/sbin/init`), sshd, python3, nftables, iproute, gcc와 Python 빌드 헤더를 설치해야 합니다.
- farm 스냅샷의 내보내기 아카이브에는 golden image 층도 들어 있어 golden image가 없는 호스트에서도 복원됩니다.
- 이 방식 이전에 Docker로 만든 `farm-golden:rocky10` 이미지의 farm과 스냅샷은 그대로 동작합니다.

## 다중 VM farm

생성 요청의 `computes`(CLI `--computes`, 포털의 **VM 구성**)가 1~3이면 control VM 1대와 compute VM N대를 만듭니다.
Nova와 그 의존성(Placement, Neutron)이 자동으로 들어갑니다.

- **VM**: `<farm>-control`(4 vCPU, 서비스에 따라 6.5 GiB 안팎)은 API·DB·MQ·네트워크 노드(L3·DHCP)와 Kolla를,
  `<farm>-compute-<n>`(2 vCPU, 3 GiB)은 nova-compute·libvirt·OVS agent를 맡습니다. 공개 포트는 control VM만 엽니다.
- **고정 정체성**: hostname과 관리망 주소는 모든 farm에서 같습니다. `farm-control` 10.254.0.10,
  `farm-compute-<n>` 10.254.0.(10+n). 서로의 이름은 `/etc/hosts`에 넣습니다.
- **네트워크**: VM마다 링크가 두 개입니다. `farm0`(관리망, Kolla의 network·api 인터페이스)과 `farm1`(주소 없음,
  OVS `br-tenant`의 physnet1, 테넌트 VLAN 100~199)입니다. 외부망 `public`은 control VM의 physnet2입니다.
  링크는 호스트마다 하나 도는 스위치 프로세스(`<agent dir>/switch/`)에 WebSocket으로 붙습니다([docs/poc-l2-link.md](docs/poc-l2-link.md)).
- **배포**: control VM에서 Kolla multinode inventory로 compute VM에 SSH 배포하고, verify는 모든 nova-compute가
  up인지 확인한 뒤 Cirros를 띄웁니다.
- **장애 주입**: `PUT /api/v1/farms/{id}/nodes/{node}/faults`(`{"loss":1}`, `{"delay_ms":50}`, `{}`로 복구),
  CLI `farm fault <farm> compute-1 --loss 1`, 포털 VM topology의 **고립·손실·지연·복구** 버튼.
  VM의 두 링크에 함께 걸립니다.
- **제약**: 로컬 호스트와 Linux 원격 호스트에서만 만들 수 있습니다. 게스트는 링크에 Linux farm-agent를 쓰는데,
  Linux 호스트는 agent 자신을 넣고 macOS 로컬 호스트는 `dist/agent/<arch>/farm-agent`(`task agent-build`)를 넣습니다.
  스냅샷은 아직 지원하지 않습니다. 공유 스토리지가 없어 evacuate는 이미지로 인스턴스를 다시 만듭니다.

```bash
farm create mn --host-id local --computes 2      # 약 12.5 GiB
farm deploy mn                                   # M1 Max에서 약 15분
farm fault mn compute-1 --loss 1                 # compute-1 고립
# control VM에서: compute service set --down, server evacuate --host farm-compute-2 ...
farm fault mn compute-1                          # 복구
```

## 웹포탈

Tailwind CSS·Base UI와 TanStack Router/Query/Table로 구성한 웹포탈은 `web/`에 있습니다.
Farm 목록·생성·배포/재개·삭제, VM 토폴로지, 실시간 로그, 작업 내역, 호스트 상태를 제공합니다.
메뉴는 `Hosts → Farms → Jobs` 순서입니다. Coordinator(`farm serve`)는 API·상태·작업만 맡는
control plane이고, VM은 **등록하고 준비한 호스트**에서만 실행합니다. coordinator가 도는 머신도 다른 서버와
같은 호스트(로컬 호스트)로 등록합니다. `Hosts`의 **로컬 호스트 등록**은 SSH 없이 등록한 뒤 점검하고, **원격 호스트 등록**은
SSH 정보 등록·점검·msb/이미지 준비·실행기 설치를 거칩니다. 어느 쪽이든 Coordinator는 그 호스트의
farm-agent에게만 VM 작업을 맡깁니다(원격은 SSH, 로컬 호스트는 같은 바이너리를 `farm agent`로 실행).
준비된 호스트의 **이 호스트에 Farm 만들기**를 누르거나 Farm 생성 시 실행 호스트를 선택하면
VM 1대의 생성·조회·Kolla 배포·삭제를 해당 호스트에서 실행합니다. Farm 안의 컴포넌트는 같은 VM을 공유합니다.
로컬 호스트의 환경 준비는 msb 0.7.13을 설치하거나 낮은 버전을 업그레이드하고 시험 VM을 부팅합니다.
golden image는 아래 [Golden image](#golden-image)처럼 호스트의 첫 farm이 msb로 빌드합니다.
**에이전트 호스트 등록**은 호스트의 agent가 coordinator로 먼저 접속하는 방식입니다. 등록하면 받는 토큰으로
호스트에서 `FARM_AGENT_TOKEN=<token> farm agent connect https://<coordinator>`(Linux 서버는 `farm-agent connect`)를
실행해 두면, SSH나 열린 포트 없이 NAT 뒤의 머신이나 외부에 둔 coordinator에서도 같은 방식으로 Farm을 실행합니다.

```bash
# 터미널 1: 프로젝트 루트
cargo run -- serve

# 터미널 2
cd web
npm ci
npm run dev                     # http://127.0.0.1:5180
```

운영 실행은 `web/`에서 `npm run build && npm start`입니다 (기본 `127.0.0.1:3000`).
API 주소와 token은 `web/.env`의 `FARM_API_URL`, `FARM_API_TOKEN`으로 설정합니다.
개발·운영 서버가 API를 프록시하고 token과 관리자 인증정보를 브라우저에서 제외합니다.
자세한 실행 방법과 서버 구성은 [web/README.md](web/README.md)를 참고하세요.
원격 실행에는 호스트 플랫폼(`linux-x86_64`·`linux-aarch64`·`macos-aarch64`·`windows-x86_64`)의 실행기가 필요합니다.
릴리스 태그로 빌드한 coordinator는 릴리스가 R2에 올린 같은 build id의 실행기를 쓰므로 따로 빌드할 필요가 없습니다.
그 밖의 빌드는 실행기를 직접 빌드하세요.

```bash
sh scripts/build-agent.sh       # Docker/BuildKit 필요; 현재 아키텍처의 dist/agent/<arch>/farm-agent
sh scripts/build-agent.sh macos-aarch64    # 또는 x86_64, aarch64, windows-x86_64
cargo run -- serve              # dist/agent 아래 모든 실행기를 읽음
```

Hosts에서 환경 준비를 완료하면 실행기도 설치합니다. 이미 준비한 호스트는 `Farm 실행기 설치`를 누르세요.
실행기에는 빌드 리비전이 박혀 있어서 coordinator와 같은 소스로 빌드한 실행기만 설치합니다.
`task dev`는 coordinator를 빌드한 뒤 그 build id가 릴리스되지 않았으면 `task agent-build`로 실행기 아티팩트도 만들고
(소스가 그대로면 건너뜀, 릴리스된 build id면 R2의 실행기를 받으므로 빌드하지 않음),
`farm serve`는 시작할 때 다른 빌드의 실행기가 설치된 SSH 호스트에 현재 실행기를 자동으로 다시 설치합니다.
`task dev` 없이 coordinator만 다시 빌드했다면 `task agent-build` 후 재시작하세요. 버전이 다르면 Hosts
체크리스트의 **Farm 실행기 버전**에 표시되고, 설치와 Farm 생성이 그 리비전을 안내하며 거절됩니다.
그다음 Farm 생성 화면의 `실행 호스트`에서 선택합니다. CLI는
`farm create remote-demo --host-id worker-01 --external-host worker-01.example` 형태입니다.
원격 호스트의 설치 조건·지문 검증·암호화 키 보관은 [docs/remote-hosts.md](docs/remote-hosts.md)를 참고하세요.

## API

```bash
# VM 생성은 바로 job을 반환하고, OpenStack 배포는 별도로 요청합니다.
curl -sS -X POST http://127.0.0.1:8080/api/v1/farms \
  -H 'Content-Type: application/json' \
  -d '{"id":"demo","external_host":"localhost","owner":"portal"}'

curl -sS http://127.0.0.1:8080/api/v1/jobs/<job-id>
# create job이 succeeded가 된 뒤:
curl -sS -X POST http://127.0.0.1:8080/api/v1/farms/demo/deploy \
  -H 'Content-Type: application/json' -d '{}'
curl -N http://127.0.0.1:8080/api/v1/farms/demo/logs/stream
```

| Method | Path (`/api/v1` 기준) | 반환 |
|---|---|---|
| GET | `/host` | Coordinator 정보: 빌드, 실행기 artifact, 상태 디렉터리 (호스트 준비 상태는 `/hosts`) |
| GET / POST | `/hosts` | 원격 호스트 목록 / SSH 정보 등록 (201) |
| POST | `/hosts/fingerprint` | 인증 전에 서버 SHA256 지문 조회 |
| GET / PUT / DELETE | `/hosts/{id}` | 호스트 상세 / 인증 정보 변경 / 등록 해제 |
| POST | `/hosts/{id}/check`, `/hosts/{id}/prepare`, `/hosts/{id}/agent` | 점검 / 환경 준비 / 실행기 설치 job |
| GET | `/host-jobs`, `/host-jobs/{id}` | 원격 호스트 작업과 단계 기록 |
| GET | `/catalog` | 실제 지원 컴포넌트·프로필·제약 |
| POST | `/plans` | 노드·게스트 메모리·디스크·경고 계산 |
| POST | `/local-host` | 로컬 호스트 등록 (`{"id":"local"}`, SSH 없음) |
| POST | `/agent-hosts` | 접속해 오는 호스트 등록 (`{"id":"edge"}`) → 1회만 표시되는 agent 토큰 |
| GET (WebSocket) | `/agent/connect` | 호스트 agent의 접속 (API token 대신 호스트 토큰) |
| GET / POST | `/farms` | 목록 / 생성 job (`host_id` 필수) |
| GET / DELETE | `/farms/{id}` | farm 상태 / 삭제 job |
| POST | `/farms/{id}/deploy` | 배포 job (`{"from":"prechecks"}`로 재개) |
| POST | `/farms/{id}/settle` | VM 재시작 뒤 서비스 응답 확인 job |
| GET | `/farms/{id}/endpoints` | public 서비스 URL과 admin 인증정보 |
| PUT | `/farms/{id}/nodes/{node}/faults` | 다중 VM farm의 VM 링크에 지연·손실 설정(`{}`이면 복구), 갱신된 farm |
| GET | `/farms/{id}/logs?after=0&limit=1000` | 최근 로그 |
| GET | `/farms/{id}/logs/stream` | 로그 SSE (`Last-Event-ID`로 재연결) |
| GET (WebSocket) | `/farms/{id}/shell?rows=24&cols=80[&node=compute-1]` | VM의 root 로그인 셸 (다중 노드는 `node`로 VM 선택) |
| POST | `/farms/{id}/snapshots` | VM을 스냅샷으로 얼리는 job |
| GET / DELETE | `/snapshots`, `/snapshots/{id}` | 스냅샷 목록·상세 / 삭제 |
| POST | `/snapshots/{id}/export`, `/snapshots/import` | `.msb` 아카이브 내보내기 / 가져오기 (파일 또는 S3 버킷) |
| GET / POST / PUT / DELETE | `/buckets`, `/buckets/{id}` | 스냅샷을 옮길 S3 호환 버킷 등록·관리 |
| GET | `/jobs`, `/jobs/{id}` | 작업 상태·phase·오류 |
| GET | `/openapi.json` | 생성된 OpenAPI 스키마 |

Farm 작업 생성은 `202 Accepted`와 `Location: /api/v1/jobs/{id}`를 반환합니다.
원격 호스트 작업의 Location은 `/api/v1/host-jobs/{id}`입니다.
포탈 연동 상세는 [docs/portal-api.md](docs/portal-api.md), 정적 스키마는
[docs/openapi.json](docs/openapi.json)을 참고하세요. `farm openapi`로 다시 생성할 수 있습니다.

## 상태와 작업

- 상태 파일: `$XDG_DATA_HOME/farm/state.json` 또는 `~/.local/share/farm/state.json`.
  `--state-dir`로 변경할 수 있지만, **한 호스트의 운영 인스턴스는 같은 디렉터리를 사용**해야 합니다.
- 파일 잠금·임시 파일·fsync·rename으로 단일 writer와 atomic write를 보장합니다.
- 시작 시와 30초마다 msb의 `farm`·`farm.role` 라벨을 조회합니다. 기존 VM도 가져오고,
  사라지거나 정지된 VM이 있으면 `broken`으로 표시합니다. 조회 실패를 VM 삭제로 취급하지 않습니다.
- VM 생성과 배포는 각각 1개씩 실행합니다. 서로 다른 farm의
  생성과 배포는 병행할 수 있습니다. 같은 farm의 중복 작업은 `409 Conflict`입니다.
- 기본 메모리 예약 한도는 호스트 RAM의 80%입니다. `--memory-budget-mib`로 조절합니다.
  생성 요청을 받는 시점에 전체 게스트 메모리를 예약하고, 삭제 성공 후 반환합니다.
  가져온 기존 farm도 VM 설정의 메모리를 계산합니다.
- 배포는 호스트에서 데몬과 분리된 worker 프로세스로 실행합니다. 데몬이 종료·재시작되거나 호스트와의
  연결이 끊겨도 배포는 끝까지 진행됩니다. 다시 시작한 데몬은 실행 중이던 배포 job을 `running`으로 두고
  (`Coordinator restarted; re-attaching to the running deploy`) 받지 못한 진행 로그부터 이어 받아
  평소처럼 마무리합니다. 호스트에 10분 동안 다시 연결하지 못하면 job은 `interrupted`가 되며,
  이때 배포는 호스트에서 아직 진행 중일 수 있으니 호스트를 확인한 뒤 재시도합니다.
- 그 밖의 job과 아직 시작하지 않은(대기 중) 배포는 데몬이 갑자기 종료되면 다음 시작 시 `interrupted`가 됩니다.
  자동 재실행하지 않습니다. 배포는 저장된 phase를 확인하고 `from`을 지정해 재개합니다.
  일부 VM만 생성된 farm은 삭제한 뒤 다시 만듭니다.
- SIGINT/SIGTERM/SIGHUP은 새 작업 접수를 멈추고 배포를 제외한 진행 중 작업을 기다린 뒤 종료합니다.
  진행 중인 배포는 기다리지 않고 호스트에서 계속되며, 다음 시작 때 다시 붙습니다.
  두 번째 신호는 즉시 종료합니다. VM은 데몬 종료 후에도 유지됩니다.
- `owner`, `expires_at`은 metadata입니다. TTL 자동 삭제는 아직 수행하지 않습니다.
- 호스트 로그는 `logs/<farm>.jsonl`에 보관하며, API는 최근 1,000개 이벤트를 제공합니다.
  재연결 시 보관 범위를 벗어난 cursor는 SSE `gap` 이벤트로 알립니다. 로그 회전은 아직 없습니다.
  Kolla의 guest 로그는 기존처럼 `/var/log/farm/<phase>.log`에도 남습니다.

## 외부 접속

관리 API와 OpenStack 서비스는 모두 기본 `127.0.0.1`에 바인딩됩니다.
원격 포탈 backend에서 접근하려면 API token과 서비스 바인딩을 지정합니다.

```bash
export FARM_API_TOKEN='<운영용-token>'
farm serve --bind 0.0.0.0:8080 --public-bind 0.0.0.0
# POST /farms에서 external_host를 이 호스트의 DNS 이름/IPv4로 지정
# 이후 API 호출에 Authorization: Bearer <운영용-token> 헤더 추가
```

loopback 이외의 관리 API 바인딩에는 token이 필수입니다. TLS는 사설망의 reverse proxy에서
종료하도록 구성하세요. 포탈 backend가 farmd와 OpenStack API를 호출하고, 브라우저에 admin
인증정보를 전달하지 않는 구조를 전제로 합니다. CORS는 활성화하지 않습니다.

farm별 public TCP block은 `15000 + 20 × n`으로 20개 포트씩 예약하고(`PUBLIC_PORT_BLOCK`),
선택한 서비스만 all-in-one VM에 직접 전달합니다. offset 6~19는 추가 서비스를 위해 비워 둡니다.
블록 폭을 넘는 서비스를 추가하면 카탈로그 테스트가 실패합니다.
HAProxy·keepalived·ProxySQL은 사용하지 않습니다.

| offset | 서비스 | VM 포트 |
|---:|---|---:|
| 0 | Keystone (`/v3`) | 5000 |
| 1 | Glance | 9292 |
| 2 | Placement | 8780 |
| 3 | Horizon (`/`) | 80 |
| 4 | Neutron | 9696 |
| 5 | Nova (`/v2.1`) | 8774 |
| 6 | MariaDB | 3306 |
| 7 | RabbitMQ (AMQP) | 5672 |
| 8 | RabbitMQ 관리 UI | 15672 |

6~8번은 모든 farm에 설치되는 데이터 스토어입니다. 배포가 끝나면 `GET /farms/{id}/endpoints`의
`datastores`와 포탈의 **Data stores** 패널에서 계정·비밀번호·데이터베이스 목록을 확인합니다.
이 릴리스 전에 만든 farm은 이 포트를 공개하지 않으므로, 필요하면 farm을 다시 만들거나
게스트의 `/etc/farm/identity.conf`와 `msb modify --port`로 직접 추가합니다.

서비스 카탈로그의 public URL에는 `external_host`와 호스트 포트를 기록하고,
내부 통신은 VM의 고정 서비스 주소(`10.254.0.2`)와 기존 서비스 포트를 사용합니다.
외부 호스트 변경이나 포트 재할당은 현재 지원하지 않습니다.

관리 계층 추가 전에 생성된 VM은 원래 포트 설정으로 가져오므로 `exposure: null`입니다.
기존 다중 VM Farm은 조회·인증정보 조회·삭제를 유지하지만 새 배포 엔진으로 재배포할 수 없습니다.
자동 변환·자동 삭제하지 않으므로 필요한 데이터를 보관한 뒤 새 Farm을 만드세요.

## 메모리

| 구성 | 메모리 (MiB) |
|---|---:|
| Keystone·Glance 기본 | 3072 |
| Placement 추가 | +256 |
| Horizon 추가 | +512 |
| Neutron 추가 | +2304 |
| Nova 추가 | +2048 |
| 현재 지원 서비스 전부 | 8192 |

모든 컴포넌트가 VM 1대(2 vCPU)의 메모리를 공유하며, 선택한 서비스만큼 메모리가 증가합니다.
메모리 값은 예약 계획이며 실제 사용량은 서비스·캐시에 따라 달라집니다. root disk는 20 GiB,
Docker와 containerd용 owned ext4 disk는 각각 32 GiB입니다(합계 84 GiB, sparse).
MariaDB 버퍼풀은 512 MiB로 고정하고 API/RPC worker는 1개씩 사용합니다.
배포도 VM 안에서 로컬 Ansible로 실행하므로 별도 deployer·SSH 키 배포·VXLAN mesh가 없습니다.

### Neutron과 게스트 커널

msb 게스트 커널에는 모듈 로딩이 없고 openvswitch datapath, `xt_connmark`, `physdev`가 없습니다.
그래서 Neutron은 다음처럼 구성합니다.

- Open vSwitch는 userspace(`netdev`) datapath로 실행합니다. 커널 datapath보다 느립니다.
  Kolla의 openvswitch 모듈 로드 task와 vswitchd 이미지의 `modprobe`는 건너뛰고,
  `br-ex`도 `datapath_type=netdev`로 생성하도록 kolla-ansible을 패치합니다.
  Nova가 요구하는 `br_netfilter` 모듈 로드도 건너뜁니다. bridge netfilter는 커널에 내장되어 있습니다.
- Nova compute는 게스트에 `/dev/kvm`이 있으면 `virt_type=kvm`과 `cpu_mode=host-passthrough`로,
  없으면 `virt_type=qemu`(TCG)와 `cpu_mode=none`으로 실행합니다. TCG 인스턴스 부팅은 KVM보다 느립니다.
  farm은 VM을 `nested_virt(true)`로 만들고, 호스트가 nesting을 지원하지 않으면 nesting 없이 다시 만듭니다.
  콘솔 프록시는 켜지 않습니다.
- 보안 그룹은 hybrid iptables 대신 `openvswitch` firewall driver를 사용합니다.
- L3 agent는 address scope용 CONNMARK 규칙만 빼고 실행합니다(`src/kolla/neutron_sitecustomize.py`).
  farm은 address scope를 설정하지 않습니다.
- 외부망 `public`(172.24.4.0/24, flat `physnet1`)은 VM의 veth `farm-ext`/`farm-extgw`
  뒤에 있습니다. 해당 VM이 172.24.4.1로 응답하고 eth0으로 masquerade합니다.
  `farm-external.service`가 재부팅 때마다 다시 구성합니다.

게스트는 남는 램을 페이지 캐시로 채우므로 실제 사용량은 할당량의 합까지 늘어날 수 있습니다.
디스크 계획에는 root disk와
Docker/containerd용 ext4 디스크 두 개가 포함됩니다. sparse disk의 현재 사용량과는 다릅니다.

## 코드와 검증

`src/lib.rs`가 라이브러리 진입점입니다. `FarmManager`가 계획·상태·예약·작업을 관리하고,
`src/manager/backend.rs`가 microsandbox VM 로직을 연결합니다. `src/api.rs`와 `src/main.rs`는
같은 manager를 사용합니다. 새 VM 이름은 farm id와 같고(`<farm>`), 이전 버전이 만든 `<farm>-all-in-one` VM도 그대로 인식합니다.

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

관리/API 테스트는 fake backend로 작업 실패·재개·배포 동시성·재시작 복구와 배포 재연결·메모리 예약·
파일 잠금·저장 실패·인증·SSE를 검증하며 VM을 띄우지 않습니다.

farm VM 사이의 L2 링크(`src/link/`, [docs/poc-l2-link.md](docs/poc-l2-link.md))는 실제 VM 3대로 확인합니다.
게스트용 Linux farm-agent가 필요합니다.

```bash
sh scripts/build-agent.sh aarch64
FARM_GUEST_AGENT=$PWD/dist/agent/aarch64/farm-agent \
  cargo test --release --test farm_link -- --ignored --nocapture
```

실제 OpenStack 설치는 별도의 로컬 Farm으로 확인합니다. 기본 구성은 VM 1대에
3 GiB를 할당하며, 최초 실행에는 패키지와 컨테이너 이미지 다운로드가 필요합니다.
기존 daemon과 다른 상태 디렉터리를 사용하세요.

```bash
cargo build --bin farm
state_dir=$(mktemp -d /tmp/farm-openstack.XXXXXX)
target/debug/farm --state-dir "$state_dir" create e2e
# 선택 서비스까지 확인하려면: create e2e --components placement,horizon
target/debug/farm --state-dir "$state_dir" deploy e2e
python3 scripts/verify-openstack.py e2e --state-dir "$state_dir"
# 로컬 VM 1대를 재시작하고 토큰·업로드한 이미지의 영속성까지 확인하려면:
python3 scripts/verify-openstack.py e2e --state-dir "$state_dir" --restart-local-vms
target/debug/farm --state-dir "$state_dir" delete e2e
```

배포의 `verify` 단계는 VM 내부에서 토큰 발급과 Glance 이미지 업로드·다운로드 내용
비교·삭제를 검사합니다. Placement는 인증된 resource provider 조회, Horizon은 브라우저와 같은
CSRF 로그인과 로그인 후 대시보드 응답까지 확인합니다. Nova가 없는 farm의 Horizon은 로그인 후
Compute 개요(403) 대신 이미지 목록으로 이동하도록 설정합니다. Neutron은 4개 agent 상태를 확인하고,
임시 tenant network·subnet·router를 만들어 동일 VM에서 라우터 게이트웨이를, 라우터
namespace에서 DHCP 포트를 ping한 뒤 지웁니다. `scripts/verify-openstack.py`는 호스트에서 공개 포트로 접속해
인증·서비스 카탈로그·프로젝트 조회·인증 없는 요청 거부·1 MiB 이미지 왕복과 삭제 후
404 응답을 검사합니다. 인증정보는 CLI에서 메모리로만 받아 사용하며 출력하거나 파일에
저장하지 않습니다. 실패 시 배포 로그를 확인한 뒤 `deploy e2e --from <phase>`로 재개할 수 있습니다.

Kolla의 기본 Ansible 출력에는 서비스 비밀값이 포함될 수 있어 Farm은 작업명과
호스트별 결과만 전달하는 callback을 사용합니다. 실패 상세는 all-in-one VM의
`/var/log/farm/ansible-failures.jsonl`에 root 전용 권한(`0600`)으로 남기며,
Ansible의 `no_log` 작업은 이 파일에서도 내용을 숨깁니다.

단일 VM 전환의 구조와 호환성은 [docs/single-vm.md](docs/single-vm.md), 이전 다중 VM 실배포 기록은
[docs/local-validation.md](docs/local-validation.md)에 있습니다.

## 릴리스

작업은 `dev`(기본 브랜치)에서 하고, `main`에 push될 때마다
[`.forgejo/workflows/release.yml`](.forgejo/workflows/release.yml)이 릴리스를 만듭니다.

- 기본은 patch입니다. 직전 태그 이후 커밋 메시지에 `#minor`/`[minor]`나 `#major`/`[major]`가
  있으면 그만큼 올립니다. Actions의 수동 실행(`workflow_dispatch`)에서도 bump를 고를 수 있습니다.
- `scripts/set-version.sh`로 `Cargo.toml`·`Cargo.lock`·`web/package.json`·`web/package-lock.json`의
  버전을 바꿔 `Release vX.Y.Z` 커밋을 `main`에 올리고, `vX.Y.Z` 태그와 Forgejo 릴리스(직전 태그 이후
  커밋 목록)를 만든 뒤 `main`을 `dev`에 다시 merge합니다.
- `Cargo.toml`의 버전에 아직 태그가 없으면(첫 릴리스나 `dev`에서 직접 올린 경우) 올리지 않고 그 버전으로
  릴리스합니다.
- 버전 커밋은 `Cargo.toml`을 바꾸므로 릴리스마다 `FARM_BUILD_ID`도 바뀝니다. 새 릴리스를 배포하면
  원격 호스트의 agent를 다시 설치해야 합니다.
- 릴리스 커밋에서 `scripts/build-agent.sh`로 linux x86_64·aarch64, macOS aarch64, Windows x86_64
  farm-agent를 빌드해 R2 `public-bucket`의 `farm-agent/<build_id>/<os>-<arch>/`에 올리고, 플랫폼별 sha256을
  담은 `manifest.json`과 `farm-agent/v<version>.json`·`latest.json`(버전 → build id)도 올립니다.
  릴리스 노트에는 `https://public-bucket.preinpost.in/farm-agent/...` 링크가 들어갑니다. 같은 태그로 빌드한
  coordinator는 이 agent를 그대로 받아들입니다. 빌드가 실패하거나 agent들의 build id가 다르면 아무것도
  push하지 않습니다.
- coordinator는 `farm serve`와 `farm host prepare`를 시작할 때 `FARM_AGENT_URL`(기본
  `https://public-bucket.preinpost.in/farm-agent`)에서 자기 build id의 `manifest.json`을 읽고, `dist/agent`에 없는
  플랫폼의 실행기를 호스트에 처음 설치할 때 받아 sha256과 build id를 확인한 뒤 `<state-dir>/agents/<sha256>/`에
  보관합니다. 릴리스되지 않은 빌드는 manifest가 없어 건너뜁니다. `FARM_AGENT_URL=`(빈 값)이면 끕니다.
- macOS와 Windows agent는 `docker/agent-cross.Dockerfile`(cargo-zigbuild, zig 0.16)로 Linux에서 cross 빌드합니다.
  macOS 링크에는 MacOSX.sdk의 `.tbd` 파일이 필요해 로컬에서는 Xcode SDK를 쓰고(`MACOS_SDK`로 바꿀 수 있음),
  CI는 비공개 Forgejo 패키지 `macos-sdk`에서 받습니다. 시크릿: `CLOUDFLARE_API_TOKEN`·`CLOUDFLARE_ACCOUNT_ID`(R2 업로드),
  `PACKAGE_TOKEN`(`read:package` 권한, SDK 다운로드).
