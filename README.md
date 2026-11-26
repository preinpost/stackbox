# stackbox

Stackbox마다 microsandbox VM을 만들고 Kolla로 OpenStack을 설치하는 Rust CLI와 REST API 데몬입니다.
새 stackbox는 **control 1 + compute 1~3**으로 만들어지며 기본은 compute 1대입니다. VM들은 호스트의 링크
스위치로 이어지고, compute VM을 고립시켜 evacuate 같은 장애 대응을 시험할 수 있습니다
([다중 VM stackbox](#다중-vm-stackbox)).
지원하는 OpenStack 릴리스는 **2024.1(Caracal) 이상**입니다([릴리스로 만들기](#릴리스로-만들기)).
웹포탈 backend는 `stackbox serve`의 API를 호출해 stackbox 생성·배포·삭제 작업과 로그를 추적할 수 있습니다.

항상 **Keystone + Glance**를 배포하고, 선택 서비스로 **Placement**, **Horizon**, **Neutron**,
**Nova**를 추가할 수 있습니다. 선택한 서비스의 의존성은 자동으로 포함됩니다.
Nova는 microsandbox nested virtualization으로 게스트에 `/dev/kvm`이 있으면 KVM으로,
없으면 qemu TCG로 작은 인스턴스만 실행합니다.

## 시작

### 필수 도구

- **msb**: microsandbox 런타임. [microsandbox 릴리스](https://github.com/preinpost/microsandbox/releases)에서
  받아 `~/.microsandbox/bin/msb`에 두거나 `MSB_PATH`로 지정합니다.
- **libkrunfw**: msb가 쓰는 커널 라이브러리. `~/.microsandbox/lib`에 두거나 `MSB_LIBKRUNFW_PATH`로 지정합니다.
- **go-task**: `task` 명령(`Taskfile.yaml`). <https://taskfile.dev>
- **rustup**: 이 저장소의 Rust 툴체인(`rust-toolchain`/CI 기준 1.99.1)을 설치합니다. <https://rustup.rs>
- **node 22**: 웹포탈(`web/`) 빌드와 실행용(`>=22.12`).
- macOS에서는 호스트에서 직접 실행하고, Linux에서는 접근 가능한 `/dev/kvm`이 필요합니다.

필요한 파일이 없으면 coordinator가 GitHub Releases에서 자동으로 받습니다. 받지 않으려면 환경변수를
빈 값으로 설정하세요.

- `STACKBOX_AGENT_URL=` : stackbox-agent 자동 다운로드 끄기 (기본 `https://github.com/preinpost/stackbox/releases/download`)
- `STACKBOX_LIBKRUNFW_URL=` : libkrunfw 커널 자동 다운로드 끄기 (기본 `https://github.com/preinpost/stackbox/releases/download`)

```bash
task golden                     # 선택: golden image를 미리 빌드 (없으면 첫 stackbox가 빌드)
cargo build --release

./target/release/stackbox catalog --json
./target/release/stackbox plan --components horizon --json
./target/release/stackbox serve      # http://127.0.0.1:10010
```

포탈 없이 CLI만으로도 같은 관리 로직을 사용할 수 있습니다.
CLI는 작업이 끝날 때까지 기다립니다.

```bash
stackbox host local                 # 로컬 호스트로 등록하고 준비 (시험 VM 부팅)
stackbox host list
stackbox create demo --host-id local                      # control 1 + compute 1 (Nova 포함)
stackbox create web --host-id local --components horizon  # 선택 서비스만큼 control VM 메모리 증가
stackbox deploy demo
stackbox deploy demo --from prechecks
stackbox status demo --json
stackbox list --json
stackbox endpoints demo --json      # 배포 성공 후 명시적으로만 admin 인증정보 조회
stackbox logs demo
stackbox delete demo

stackbox snapshot create demo demo-clean     # VM을 얼려두기 (잠시 정지 후 재시작)
stackbox create demo2 --host-id local --snapshot demo-clean  # 다시 설치하지 않고 불러오기
stackbox snapshot export demo-clean -o ~/stackbox-snapshots/
stackbox snapshot import ~/stackbox-snapshots/demo-clean.msb
stackbox snapshot export demo-clean --bucket team-r2   # 등록한 S3 호환 버킷(AWS·R2·MinIO·RustFS)으로
stackbox snapshot bucket team-r2                       # 버킷 아카이브의 릴리스·역할 (메타데이터 파일과 index.json)
stackbox settle demo                         # VM 재시작 뒤 배포 없이 서비스 응답만 확인
```

스냅샷의 동작과 제약은 [docs/snapshots.md](docs/snapshots.md)를 참고하세요.

데몬과 CLI는 같은 상태 디렉터리에 대해 배타 잠금을 사용합니다.
**데몬 실행 중에는 포탈/API로 작업하고, 직접 CLI를 사용하려면 데몬을 먼저 종료**합니다.
`catalog`, `plan`, `openapi`는 잠금 없이 실행됩니다.

## Golden image

모든 stackbox VM은 호스트의 golden image에서 시작합니다. golden image는 msb 디스크 스냅샷
`stackbox-golden-<os>-<레시피 해시>:<레시피 해시>`입니다(레시피마다 msb 그룹이 따로 생깁니다). Docker도 호스트에 미리 준비할 것도 없습니다.

- 호스트에 그 스냅샷이 없으면 첫 stackbox를 만들 때 msb가 빌드합니다. 배포판 공식 OCI 이미지
  (`rockylinux/rockylinux:10`)로 VM을 띄워 `guest/os/<os>.sh`를 실행하고, VM을 멈춘 뒤 디스크(root와
  Docker·containerd용 디스크 두 개)를 스냅샷으로 뜹니다. M1 Max에서 약 35초 걸렸습니다.
  `task golden`(`stackbox golden`)으로 미리 빌드하고, `stackbox golden --check`로 확인할 수 있습니다.
- `guest/os/<os>.sh`에는 배포판 패키지 설치만 둡니다. stackbox 전용 파일(`guest/stackbox/`의 identity·hostname·
  외부망 유닛)은 VM을 만들 때와 배포할 때 넣으므로, 이 파일을 바꿔도 golden image를 다시 빌드하지 않습니다.
- 스냅샷 이름의 해시는 기본 이미지, 설치 스크립트, 디스크 크기로 정해집니다. 셋 중 하나가 바뀌면 새 이름이
  되어 다음 stackbox가 새로 빌드합니다. 옛 스냅샷은 그 위에서 만든 stackbox와 스냅샷이 있는 동안 남아 있습니다.
- 새 배포판은 `guest/os/<id>.sh`와 `src/stackbox/golden.rs`의 `GuestOs` 항목 하나로 추가합니다. 스크립트는
  systemd(`/sbin/init`), sshd, python3, nftables, iproute, gcc와 Python 빌드 헤더를 설치해야 합니다.
- stackbox 스냅샷의 내보내기 아카이브에는 golden image 층도 들어 있어 golden image가 없는 호스트에서도 복원됩니다.
- 이 방식 이전에 Docker로 만든 `stackbox-golden:rocky10` 이미지의 stackbox와 스냅샷은 그대로 동작합니다.

## 다중 VM stackbox

모든 새 stackbox는 control VM 1대와 compute VM 1~3대입니다. 생성 요청의 `computes`(CLI `--computes`, 포털의
**VM 구성**)가 그 수를 정하며 기본은 1이고, Nova와 그 의존성(Placement, Neutron)이 compute와 함께 들어갑니다.
포털의 **VM 구성**은 `Control + Compute 1대`·`2대`·`3대`만 제공합니다.

`layout`(`all-in-one`·`control`)과 `computes: 0`으로는 새 stackbox를 만들지 않습니다. 예전에 만든 stackbox와
스냅샷은 그 layout을 그대로 읽고 배포·compute 추가·삭제·복원할 수 있지만, `POST /plans`·
`POST /stackboxes`(`snapshot_id` 없이)·`POST /releases/plan`·`POST /releases/stackboxes`는 이 값들을 `400`으로
거절합니다. 스냅샷 복원(`snapshot_id`)만 얼린 layout을 그대로 따릅니다.

- **VM**: `<stackbox>-control`(4 vCPU, 서비스에 따라 6.5 GiB 안팎)은 API·DB·MQ·네트워크 노드(L3·DHCP)와 Kolla를,
  `<stackbox>-compute-<n>`(기본 2 vCPU, 3 GiB)은 nova-compute·libvirt·OVS agent를 맡습니다. 공개 포트는 control VM만 엽니다.
- **compute 사양**: 생성 요청의 `compute`(`{"cpus":4,"memory_mib":8192,"disk_mib":65536}`, 빠진 값은 기본값),
  CLI `--compute-cpus`·`--compute-memory-mib`·`--compute-disk-mib`, 포털의 **Compute VM 사양**으로 모든 compute VM의
  크기를 정합니다. nova-compute·libvirt·OVS가 약 0.8 GiB를 쓰고 나머지가 인스턴스 몫입니다. `disk_mib`는 컨테이너
  디스크 하나의 크기이고 Nova의 인스턴스 디스크가 여기에 들어갑니다. 디스크 크기는 golden image의 일부라 처음 쓰는
  크기는 호스트에서 golden image를 한 번 더 만듭니다.
- **고정 정체성**: hostname과 관리망 주소는 모든 stackbox에서 같습니다. `stackbox-control` 10.254.0.10,
  `stackbox-compute-<n>` 10.254.0.(10+n). 서로의 이름은 `/etc/hosts`에 넣습니다.
- **네트워크**: VM마다 링크가 두 개입니다. `stackbox0`(관리망, Kolla의 network·api 인터페이스)과 `stackbox1`(주소 없음,
  OVS `br-tenant`의 physnet1, 테넌트 VLAN 100~199)입니다. 외부망 `public`은 control VM의 physnet2입니다.
  링크는 호스트마다 하나 도는 스위치 프로세스(`<agent dir>/switch/`)에 WebSocket으로 붙습니다([docs/poc-l2-link.md](docs/poc-l2-link.md)).
- **배포**: control VM에서 Kolla multinode inventory로 compute VM에 SSH 배포하고, verify는 모든 nova-compute가
  up인지 확인한 뒤 Cirros를 띄웁니다.
- **장애 주입**: `PUT /api/v1/stackboxes/{id}/nodes/{node}/faults`(`{"loss":1}`, `{"delay_ms":50}`, `{}`로 복구),
  CLI `stackbox fault <stackbox> compute-1 --loss 1`, 포털 VM topology의 **고립·손실·지연·복구** 버튼.
  VM의 두 링크에 함께 걸립니다.
- **제약**: 로컬 호스트와 Linux 원격 호스트에서만 만들 수 있습니다. 게스트는 링크에 Linux stackbox-agent를 쓰는데,
  Linux 호스트는 agent 자신을 넣고 macOS 로컬 호스트는 `dist/agent/<arch>/stackbox-agent`(`task agent-build`)를 넣습니다.
  스냅샷은 아직 지원하지 않습니다. 공유 스토리지가 없어 evacuate는 이미지로 인스턴스를 다시 만듭니다.

```bash
stackbox create mn --host-id local --computes 2      # 약 12.5 GiB
# 더 큰 compute: --compute-cpus 4 --compute-memory-mib 8192 --compute-disk-mib 65536
stackbox deploy mn                                   # M1 Max에서 약 15분
stackbox fault mn compute-1 --loss 1                 # compute-1 고립
# control VM에서: compute service set --down, server evacuate --host stackbox-compute-2 ...
stackbox fault mn compute-1                          # 복구
```

### 여러 호스트에 걸친 stackbox

compute VM을 control VM과 다른 호스트에 둘 수 있습니다. 생성 요청의 `placement`(`{"compute-2":"dev-01"}`),
CLI `--place compute-2=dev-01`, 포털의 **Compute VM 배치**로 정합니다. 빠진 compute는 `host_id`(control VM의 호스트)에 둡니다.

- **연결**: 호스트마다 스위치가 stackbox별 uplink를 coordinator의 relay(`wss://…/api/v1/link-relay`)에 열고, relay가
  같은 stackbox의 다른 호스트로 프레임을 넘깁니다. 스위치에는 그 stackbox의 segment만 오갑니다.
- **설정**: 포털 **Hosts** 아래 **호스트 간 relay**(또는 `PUT /api/v1/relay`)에서 URL, TLS 방식, listen 주소를 정합니다.
  저장하면 재시작 없이 바로 다시 열립니다. 포털은 처음에 Cloudflare 임시 터널을 골라 둡니다.
  `STACKBOX_RELAY_URL`·`STACKBOX_RELAY_TLS`·`STACKBOX_RELAY_BIND`는 저장된 설정이 없을 때의 초기값이며, `STACKBOX_RELAY_URL`이 있어야
  쓰입니다. `STACKBOX_RELAY_TLS`는 `private`(기본)와 `proxy`만 받고, 다른 값은 경고를 남기고 `private`로 씁니다.
  - **Cloudflare 임시 터널**(`quick_tunnel`, 포털 기본): coordinator가 cloudflared를 직접 띄워 relay(`127.0.0.1:10012`)를
    `*.trycloudflare.com`(Cloudflare Quick Tunnel)으로 내보냅니다. 계정과 포트 개방이 필요 없습니다. Cloudflare는
    개발·시험용으로 제공하며 가용성을 보장하지 않습니다. 고정 주소가 필요하면 `proxy`로 직접 띄운 터널을 씁니다. coordinator가 다시 뜰 때마다 주소가 바뀌며,
    바뀐 주소는 여러 호스트에 걸친 stackbox의 각 호스트 스위치에 자동으로 전달됩니다(토큰과 CA는 그대로).
  - `private`: relay가 `0.0.0.0:10011`에서 coordinator 사설 CA로 TLS를 엽니다. 다른 호스트가 이 포트에 닿아야 합니다.
  - **cloudflared**: 시스템에 설치된 것은 쓰지 않습니다. GitHub 최신 릴리스를 `<state dir>/bin/`에 받아, GitHub가 공개한
    SHA-256과 대조하고 macOS에서는 Cloudflare 코드 서명(Team ID `68WVV388M8`)도 확인합니다. 하루 한 번 새 릴리스를
    확인해 바꾸고 터널을 다시 띄웁니다. 직전 버전 하나를 남깁니다. `STACKBOX_CLOUDFLARED_VERSION`으로 버전을 고정하고,
    `STACKBOX_CLOUDFLARED`로 직접 관리하는 바이너리를 쓸 수 있습니다. 로그는 `<state dir>/cloudflared.log`입니다.
  - `proxy`: relay가 `127.0.0.1:10012`에서 TLS 없이 열리고, 직접 띄운 프록시가 공인 인증서로 TLS를 맡습니다.
    스위치는 공인 CA로 검증합니다. 예: `cloudflared tunnel --url http://127.0.0.1:10012`, URL은 `https://relay.<도메인>`.
    프레임은 종단 간 암호화로 프록시에 보이지 않습니다. uplink는 30초마다 ping해 유휴 연결이 끊기지 않게 합니다.
  - 여러 호스트에 걸친 stackbox가 있는 동안에는 URL과 TLS 방식을 바꿀 수 없습니다(스위치가 예전 값을 들고 있음).
- **TLS**: coordinator가 처음 시작할 때 사설 CA를 만들어 `<state dir>/relay-tls/`에 둡니다. 시작할 때마다
  `STACKBOX_RELAY_URL`의 호스트로 서버 인증서를 새로 서명합니다. CA는 stackbox 생성 요청에 실려 각 호스트 agent로 가고,
  스위치는 그 CA만 믿습니다. CA를 지우면 기존 stackbox의 링크가 끊기므로 다시 만들어야 합니다.
- **종단 간 암호화**: TLS 방식과 상관없이 스위치와 relay 사이의 모든 메시지를 (stackbox, 호스트)마다 다른 32바이트 키로
  XChaCha20-Poly1305 암호화합니다. 터널이나 프록시가 TLS를 끝내도 암호문만 봅니다. 연결마다 양쪽이 무작위 hello를
  주고받고 순번을 붙여, 다른 연결의 메시지를 다시 보내거나 순서를 바꾸면 연결을 끊습니다. relay는 coordinator 안에서
  메시지를 풀어 segment로 보낼 곳을 정하고 받는 호스트의 키로 다시 암호화합니다. 키는 stackbox 생성 요청에 실려 각 호스트로
  가고, coordinator는 vault로 암호화해 두며 stackbox를 지우면 지웁니다.
- **인증**: (stackbox, 호스트)마다 32바이트 uplink 토큰을 만들어 그 호스트의 스위치 설정(0600)에 두고, coordinator는
  SHA-256만 저장합니다. 토큰은 그 stackbox의 segment에만 통하고, stackbox를 지우면 폐기합니다.
- **배포**: control VM의 SSH 키는 coordinator가 만들어 control VM의 호스트에만 개인 키를 보냅니다. 배포는 다른
  호스트의 compute VM을 그 호스트 agent로 먼저 준비하고(`prepare`), control VM의 호스트에서 Kolla를 돌립니다.
- **제약**: 스냅샷으로 만들 수 없습니다. 각 호스트의 실행기는 coordinator와 같은 빌드여야 합니다.

```bash
# 포털 Hosts에서 relay URL을 저장한 뒤
stackbox create span --host-id local --computes 2 --place compute-2=dev-01
stackbox deploy span
stackbox add-compute span --host-id dev-01   # 배포된 stackbox에 compute를 한 대 더
```

### 배포된 stackbox에 Compute 추가

배포가 끝난 다중 VM stackbox에는 compute VM을 한 대씩 더 붙일 수 있습니다.
`POST /api/v1/stackboxes/{id}/computes`(CLI `stackbox add-compute`)에 `host_id`를 넘기면 그 호스트에,
생략하면 stackbox의 primary 호스트에 다음 `compute-N`을 만듭니다.

- **같은 호스트**: stackbox의 링크 스위치에 포트만 추가하고, control VM의 Kolla inventory에 새 VM을 넣습니다.
- **이미 stackbox에 참여한 원격 호스트**: 그 호스트의 기존 share와 uplink를 그대로 씁니다.
- **처음 붙는 원격 호스트**: 새 share와 uplink를 만들고, stackbox가 전부 한 호스트에 있었다면
  control VM의 호스트도 그때 relay에 붙입니다. control VM의 기존 SSH 키와 공개키를 그대로 쓰므로
  이미 배포된 compute의 접속이 끊기지 않습니다.

Kolla는 passwords와 globals를 다시 만들지 않고 inventory만 갱신한 뒤, 새 compute에만
`bootstrap-servers`와 `deploy --limit stackbox-compute-N`을 실행합니다. nova cell 등록은 Kolla의
compute 배치 작업이 control의 conductor로 위임해 처리하고, 마지막에 `openstack compute service list`와
network agent로 새 compute가 올라왔는지 확인합니다. stackbox의 plan은 Kolla가 성공한 뒤에만 갱신되므로,
실패한 작업은 같은 `stackbox add-compute` 명령으로 이어서 재시도할 수 있습니다.

지금은 compute가 최대 3대이고, all-in-one stackbox는 대상이 아닙니다. 스냅샷에서 복원한 다중 VM stackbox는
복원된 plan과 control VM의 Kolla 설정을 그대로 쓰므로 똑같이 compute를 붙일 수 있습니다.
새 호스트는 control VM과 같은 CPU 아키텍처이고, 기존 stackbox가 KVM이면 `/dev/kvm`도 있어야 합니다.
기존 인스턴스는 새 compute로 자동 이동하지 않으므로 필요하면 live migration을 따로 실행합니다.

#### Compute 템플릿에서 추가

배포된 stackbox의 compute VM 하나를 **compute 템플릿**(단일 VM 스냅샷)으로 얼려두면, 다음 compute를
이미지 pull과 `bootstrap-servers` 없이 그 디스크로 부팅해 붙일 수 있습니다.

```bash
stackbox snapshot create grow grow-compute-tpl --node compute-1   # compute-1만 잠시 멈췄다가 다시 시작
stackbox add-compute grow --from-snapshot grow-compute-tpl          # 생략하면 지금처럼 클린 설치
```

- 템플릿에는 `stackbox.snapshot.role=compute`, OpenStack 릴리스, 서비스 목록, 아키텍처가 라벨로 남고,
  archive로 내보내고 가져와도 템플릿으로 남습니다. stackbox 복원에는 쓸 수 없습니다.
- 추가할 때 stackbox와 릴리스·서비스·아키텍처가 다르면 `409`입니다. 템플릿은 로컬 호스트에서 얼리지만,
  `--host-id`로 다른 호스트에 붙이면 job이 먼저 그 호스트에 복사본을 둡니다(버킷에 있으면 호스트가
  직접 내려받고, 아니면 coordinator가 agent 연결로 보냄). 다음부터는 그 복사본을 씁니다.
  [docs/snapshots.md](docs/snapshots.md#다른-호스트로-복사) 참고.
- 얼릴 때는 링크와 Docker를 디스크에서 꺼 둡니다(원본은 다시 켜서 시작). 템플릿으로 부팅한 VM은
  Docker가 뜨기 전에 Nova `compute_id`, 인스턴스와 libvirt domain, `authorized_keys`를 지우고
  machine-id를 새로 만든 뒤 Docker를 켜고 OVS system-id를 새 hostname으로 바꿉니다. 그 뒤
  `kolla-ansible reconfigure --limit stackbox-compute-N`과 `nova-manage cell_v2 discover_hosts`를 실행합니다.
  인스턴스가 있는 compute는 템플릿으로 얼릴 수 없습니다.
- 실패한 확장은 처음과 같은 방식(같은 템플릿 또는 클린 설치)으로만 재시도할 수 있습니다.

#### Control 템플릿으로 클러스터 조립

배포된 다중 VM stackbox의 control VM을 **control 템플릿**으로 얼려두면, 그 템플릿에서 compute 없는
stackbox를 만든 뒤 compute를 한 대씩(클린 설치 또는 compute 템플릿에서) 붙여 1c1c, 1c3c 같은 클러스터를
조립할 수 있습니다.

```bash
stackbox snapshot create src src-control-tpl --node control    # compute를 먼저 멈추고 control만 저장
stackbox create c13 --host-id local --snapshot src-control-tpl  # control VM 하나, compute 0대로 Ready
stackbox add-compute c13                                        # compute-1: 클린 설치
stackbox add-compute c13 --from-snapshot src-compute-tpl        # compute-2, 3: compute 템플릿에서
```

- 얼릴 때는 stackbox 전체를 얼릴 때처럼 링크를 끄고 compute → control 순서로 멈춘 뒤 control VM만
  단일 VM 스냅샷으로 저장하고, control → compute 순서로 다시 켭니다. 원본 stackbox의 OpenStack은 바꾸지
  않습니다. 인스턴스가 있거나, compute가 다른 호스트에 있는 stackbox는 `409`입니다.
- 템플릿에는 `stackbox.snapshot.role=control`, 릴리스, 서비스, 아키텍처 라벨이 남고, 스냅샷의 `plan`은
  control VM 하나(`computes: 0`)입니다. 내보내고 가져와도 control 템플릿으로 남습니다.
- 이 템플릿으로 만든 stackbox(기존 `snapshot_id` 복원 경로)은 새 스위치·포트·링크로 control VM만 부팅한 뒤,
  복사본에서만 원본 compute의 등록을 지웁니다: `openstack compute service delete`(nova-compute),
  남은 placement resource provider, `stackbox-compute-*`의 Neutron agent, `nova-manage cell_v2 delete_host`.
  Kolla inventory의 `[compute]`와 control VM의 compute SSH host key도 비우므로 첫 확장은 compute-1입니다.
- 스냅샷(control 템플릿)에서 복원한 stackbox는 다시 배포(`deploy`)할 수 없고 stackbox 전체로도 얼릴 수 없습니다(control 템플릿만).
  예전에 클린 설치로 만든 control-only stackbox(`layout: control`)은 첫 배포가 필요하고, 배포한 뒤 compute를 붙입니다.
  새 stackbox는 이 layout으로 만들 수 없으므로, compute 없이 시작하는 stackbox는 이 템플릿 복원 경로뿐입니다.
  compute 템플릿은 여전히 stackbox 복원에 쓸 수 없습니다.
- `stackbox create --host-id <원격 호스트> --snapshot <control 템플릿>`도 됩니다. compute 템플릿과 같이
  그 호스트에 복사본을 먼저 둡니다.

#### 릴리스로 만들기

포털의 **Stackbox 만들기**는 폼 하나입니다. 구성(서비스·VM 구성·compute 사양)을 직접 고르고 같은 폼에서
OpenStack 릴리스를 고르면, coordinator가 그 릴리스의 저장된 템플릿으로 조립 계획을 세우고 그대로
실행합니다. 릴리스 목록은 `kolla-ansible` 공개 저장소의 `stable/<release>`·`unmaintained/<release>`
브랜치에서 읽어 오고(10분 캐시), 저장소에 닿지 않는 호스트는 `STACKBOX_OPENSTACK_RELEASES=2026.2,2025.2`처럼
목록을 고정할 수 있습니다.

**지원 범위는 2024.1(Caracal) 이상입니다.** 릴리스 목록과 계획·생성 API는 그보다 오래된 릴리스를
받지 않습니다. 2024.1·2024.2는 kolla-ansible 브랜치나 마지막 태그는 남아 있지만
`quay.io/openstack.kolla`에 이미지가 없어, 이 저장소가 빌드해 게시한 Rocky 9 이미지
(`ghcr.io/preinpost/kolla`, amd64 전용)를 씁니다. 2024.1은 `unmaintained/2024.1`, 2024.2는
`2024.2-eol` 태그로 설치하고 이미지 태그는 각각 `2024.1-rocky-9`, `2024.2-rocky-9`입니다.
2023.1·2023.2는 지원하지 않습니다. 2025.1 이상은 `quay.io/openstack.kolla`의 그 릴리스 이미지를
그대로 씁니다. 2026.2만 Rocky 10 `aarch64` 이미지가 있어 arm64 호스트를 지원하고, 그 밖의
릴리스는 amd64 전용입니다. 2024.1의 requirements에는 Ansible이 없어 설치 단계가
ansible-core 2.16(이 릴리스가 허용하는 상한)을 넣고, 2024.2는 자체 requirements의
ansible-core 2.16~2.18을 씁니다. 두 릴리스 모두 Rocky 10 guest를 지원하지 않으므로
globals.yml에서 host OS precheck를 끄고 SELinux·Neutron 모듈 기본값을 guest에 맞춥니다.

2022.2(zed) 이하 EOL 지원 코드는 남아 있지만 기본 릴리스 목록과 계획·생성 API에서는 빠집니다.
coordinator에 `STACKBOX_ALLOW_UNSUPPORTED_RELEASES=1`을 설정한 경우에만 고를 수 있습니다. 그 시리즈는
kolla-ansible이 브랜치를 지우므로 마지막 태그(예: victoria `11.4.0`, zed `15.6.0`)로 설치하며,
포털의 릴리스 목록에는 **EOL**로 표시됩니다. 세 가지 제약이 있습니다.

- 그 시리즈의 Kolla 이미지는 **amd64 전용**입니다. aarch64 호스트(Apple Silicon 포함)에서는
  설치할 수 없고, 릴리스 목록에서도 `installable: false`로 나옵니다. x86_64 Linux 호스트에서만 됩니다.
- 그 시대 Ansible은 게스트의 Python 3.12에서 돌지 않습니다(ussuri 3.8, victoria~xena 3.9,
  yoga·zed 3.10). 게스트에 해당 `python3.x`가 없으면 설치 단계가 `/opt/stackbox-uv`의 uv로 그 minor를
  `/opt/stackbox-python`에 받아 씁니다. 골든 이미지에 `/opt/python3.9`나 `/usr/local/bin/python3.9`처럼
  미리 준비해 두면 그쪽을 먼저 씁니다.
- 설치 중 적용하는 커널 호환 패치(OVS netdev, userspace TSO, br_netfilter)는 태그마다 다른
  task 파일 구조를 따릅니다. 릴리스별 패치 목록이 바꿀 문자열을 직접 지정하고, 검토했던 파일과
  다르면 그 단계를 실패시킵니다. 조용히 건너뛰지 않습니다.

EOL 릴리스는 그 시대 도구가 전제하던 환경을 게스트에 맞춰 채웁니다. 설치 단계는
`libxcrypt-compat`(Rocky 10에 없는 `libcrypt.so.1`), 같은 가상환경의 Ansible과 `Jinja2<3.1`,
그리고 Ansible 2.9가 Python 3.12와 Rocky에서 동작하도록 필요한 것들(번들 six 1.16, setuptools의
`distutils`, Python 3.12가 지운 `http.client` 속성, OS family 표의 Rocky 항목)을 넣습니다. 태그의
`kolla-ansible`은 venv를 활성화하지 않으면 playbook을 `bin/` 옆 소스 트리에서 찾으므로
`VIRTUAL_ENV`도 함께 넘깁니다. prepare 단계는 `python3-setuptools`·`gnupg2`와 Docker SDK
4.x(requests 2.32 미만, urllib3 1.x)를 게스트의 시스템 Python에 설치하고, globals.yml에는 그
태그가 거부하는 host OS precheck를 끄는 값과 docker-ce가 게시된 `/linux/centos` 저장소를
적습니다. dnf5가 없앤 deltarpm task는 그 태그의 role에서 제거합니다.

EOL 이미지는 시리즈마다 출처가 다릅니다. zed(2022.2)는 2024.1·2024.2와 같은 방식으로 이 저장소가
빌드한 Kolla 이미지를 공개 미러 `ghcr.io/preinpost/kolla`에 `zed-rocky-9` 태그로 게시합니다
(amd64 전용, 익명 pull). 그래서 zed stackbox는 `kolla_base_distro: rocky`와 그 레지스트리를 globals.yml에
적고 이미지 안의 Python 3.9에 Neutron 패치를 넣어, deploy와 서비스 검증까지 진행합니다. 미러를
따로 두면 `STACKBOX_EOL_REGISTRY`·`STACKBOX_EOL_NAMESPACE`로 바꿀 수 있습니다. yoga 이하 시리즈는 아직 그
태그의 기본 레지스트리(`quay.io/openstack.kolla`)를 그대로 쓰며, 그곳에 EOL 이미지가 없으므로
deploy 단계에서 pull 404로 멈춥니다. 그 시리즈도 빌드해 게시하면 릴리스 표(`Legacy`)의 `built`에
base distro를 적어 켭니다.

설치 단계는 그 태그의 kolla-ansible만으로는 부족한 Ansible을 같은 가상환경에 함께 넣습니다.
태그의 `requirements.txt`가 Ansible을 선언하지 않기 때문이고, 범위는 시리즈마다 그 태그의 quickstart가
밝히는 값을 그대로 씁니다(ussuri `ansible>=2.8,<2.10`, victoria `ansible>=2.9,<2.10`, wallaby
`ansible>=2.9,<3.0`, xena `ansible>=2.10,<5.0`, yoga·zed `ansible>=4,<6`). `kolla-ansible
install-deps`는 yoga(14.11.0)에 생긴 명령이라 xena 이하 태그에는 없습니다. 그 릴리스에서는 Galaxy
의존성 단계를 건너뛰며, role은 그 태그의 저장소에 함께 들어 있습니다.

`STACKBOX_OPENSTACK_RELEASES`는 목록을 고정하며, 지원 범위 밖의 릴리스는 넣어도 무시됩니다(위 opt-in을
켜면 EOL 시리즈도 포함). 템플릿이 없는 릴리스는 그 브랜치·태그로 클린 설치하므로, 이 binary가
배포하는 릴리스가 아니어도 stackbox를 만들 수 있습니다. control VM은 그 릴리스의 control 템플릿 중 가장
최근 것(로컬 스냅샷을 먼저, 없으면 버킷의 아카이브)에서, 각 compute는 같은 릴리스·서비스의 compute
템플릿에서 만들고, 쓸 수 있는 템플릿이 없으면 클린 설치합니다.

```bash
stackbox releases                                                        # 릴리스별 control/compute 템플릿 수(로컬·버킷)
stackbox create c13 --host-id local --release 2026.2 --computes 3 --dry-run  # VM마다 템플릿/클린 설치와 이유만 출력
stackbox create c13 --host-id local --release 2026.2 --computes 3 \
  --compute-host local --compute-host local --compute-host dev-01    # compute-3만 원격 호스트
```

- 계획은 `POST /api/v1/releases/plan`(dry run), 실행은 `POST /api/v1/releases/stackboxes`입니다. 실행은
  `assemble` job 하나로 묶이며, 단계(`assembly`)마다 기존 job이 차례로 돕니다: 버킷 템플릿 가져오기 →
  control 템플릿 복원(`restore`) → compute마다 `expand`. 같은 아카이브를 가져오는 중인 import가 있으면
  새로 받지 않고 기다립니다.
- 다른 호스트에 둔 control이나 compute도 템플릿으로 부팅합니다. restore·expand 단계가 템플릿을 그
  호스트에 먼저 복사하고, 이미 복사본이 있으면 그대로 씁니다(아래 "다른 호스트로 복사").
- control 템플릿이 없거나 쓸 수 없으면 클린 설치가 가능한(`installable`) 릴리스만 기존 경로대로
  클린 생성(`create`) → 배포(`deploy`)하고, 그 밖의 릴리스는 `409`로 거절합니다.
- 중간 단계가 실패하면 stackbox는 그때까지 붙은 VM을 유지합니다. 실패한 compute는 job 오류에 적힌 대로
  `stackbox add-compute`(같은 템플릿 또는 클린 설치)로 재시도하고 나머지도 이어 붙이면 됩니다.

## 웹포탈

Tailwind CSS·Base UI와 TanStack Router/Query/Table로 구성한 웹포탈은 `web/`에 있습니다.
Stackbox 목록·생성·배포/재개·삭제, VM 토폴로지, 실시간 로그, 작업 내역, 호스트 상태를 제공합니다.
메뉴는 `Hosts → Stackboxes → Jobs` 순서입니다. Coordinator(`stackbox serve`)는 API·상태·작업만 맡는
control plane이고, VM은 **등록하고 준비한 호스트**에서만 실행합니다. coordinator가 도는 머신도 다른 서버와
같은 호스트(로컬 호스트)로 등록합니다. `Hosts`의 **로컬 호스트 등록**은 SSH 없이 등록한 뒤 점검하고, **원격 호스트 등록**은
SSH 정보 등록·점검·msb/이미지 준비·실행기 설치를 거칩니다. 어느 쪽이든 Coordinator는 그 호스트의
stackbox-agent에게만 VM 작업을 맡깁니다(원격은 SSH, 로컬 호스트는 같은 바이너리를 `stackbox agent`로 실행).
준비된 호스트의 **이 호스트에 Stackbox 만들기**를 누르거나 Stackbox 생성 시 실행 호스트를 선택하면
VM 1대의 생성·조회·Kolla 배포·삭제를 해당 호스트에서 실행합니다. Stackbox 안의 컴포넌트는 같은 VM을 공유합니다.
로컬 호스트의 환경 준비는 msb 0.8.1을 설치하거나 낮은 버전을 업그레이드하고, stackbox 게스트 커널을
설치한 뒤 그 커널로 시험 VM을 부팅해 VFIO·mdev·mtty까지 확인합니다. 실행기는 coordinator 자신이라
따로 설치하지 않습니다.
golden image는 아래 [Golden image](#golden-image)처럼 호스트의 첫 stackbox가 msb로 빌드합니다.
**에이전트 호스트 등록**은 호스트의 agent가 coordinator로 먼저 접속하는 방식입니다. 등록하면 받는 토큰으로
호스트에서 `STACKBOX_AGENT_TOKEN=<token> stackbox agent connect https://<coordinator>`(Linux 서버는 `stackbox-agent connect`)를
실행해 두면, SSH나 열린 포트 없이 NAT 뒤의 머신이나 외부에 둔 coordinator에서도 같은 방식으로 Stackbox를 실행합니다.

```bash
# 터미널 1: 프로젝트 루트
cargo run -- serve

# 터미널 2
cd web
npm ci
npm run dev                     # http://127.0.0.1:10020
```

운영 실행은 `web/`에서 `npm run build && npm start`입니다 (기본 `127.0.0.1:10020`).
API 주소와 token은 `web/.env`의 `STACKBOX_API_URL`, `STACKBOX_API_TOKEN`으로 설정합니다.
개발·운영 서버가 API를 프록시하고 token과 관리자 인증정보를 브라우저에서 제외합니다.
자세한 실행 방법과 서버 구성은 [web/README.md](web/README.md)를 참고하세요.
원격 실행에는 호스트 플랫폼(`linux-x86_64`·`linux-aarch64`·`macos-aarch64`·`windows-x86_64`)의 실행기가 필요합니다.
릴리스 태그로 빌드한 coordinator는 릴리스가 R2에 올린 같은 build id의 실행기를 쓰므로 따로 빌드할 필요가 없습니다.
그 밖의 빌드는 실행기를 직접 빌드하세요.

```bash
sh scripts/build-agent.sh       # Docker/BuildKit 필요; 현재 아키텍처의 dist/agent/<arch>/stackbox-agent
sh scripts/build-agent.sh macos-aarch64    # 또는 x86_64, aarch64, windows-x86_64
cargo run -- serve              # dist/agent 아래 모든 실행기를 읽음
```

Hosts에서 환경 준비를 완료하면 실행기도 함께 설치합니다. 이미 준비한 호스트에서 실행기 버전이
바뀌었거나 준비가 필요하면 **환경 준비**를 다시 실행하세요.
실행기에는 빌드 리비전이 박혀 있어서 coordinator와 같은 소스로 빌드한 실행기만 설치합니다.
`task dev`는 coordinator를 빌드한 뒤 그 build id가 릴리스되지 않았으면 `task agent-build`로 실행기 아티팩트도 만들고
(소스가 그대로면 건너뜀, 릴리스된 build id면 R2의 실행기를 받으므로 빌드하지 않음),
`stackbox serve`는 시작할 때 다른 빌드의 실행기가 설치된 SSH 호스트에 현재 실행기를 자동으로 다시 설치합니다.
`task dev` 없이 coordinator만 다시 빌드했다면 `task agent-build` 후 재시작하세요. 버전이 다르면 Hosts
체크리스트의 **Stackbox 실행기 버전**에 표시되고, **환경 준비**를 다시 실행하면 이 빌드의 실행기로 바뀝니다.
설치와 Stackbox 생성이 그 리비전을 안내하며 거절됩니다.
그다음 Stackbox 생성 화면의 `실행 호스트`에서 선택합니다. CLI는
`stackbox create remote-demo --host-id worker-01 --external-host worker-01.example` 형태입니다.
원격 호스트의 설치 조건·지문 검증·암호화 키 보관은 [docs/remote-hosts.md](docs/remote-hosts.md)를 참고하세요.

## API

```bash
# VM 생성은 바로 job을 반환하고, OpenStack 배포는 별도로 요청합니다.
curl -sS -X POST http://127.0.0.1:10010/api/v1/stackboxes \
  -H 'Content-Type: application/json' \
  -d '{"id":"demo","external_host":"localhost","owner":"portal"}'

curl -sS http://127.0.0.1:10010/api/v1/jobs/<job-id>
# create job이 succeeded가 된 뒤:
curl -sS -X POST http://127.0.0.1:10010/api/v1/stackboxes/demo/deploy \
  -H 'Content-Type: application/json' -d '{}'
curl -N http://127.0.0.1:10010/api/v1/stackboxes/demo/logs/stream
```

| Method | Path (`/api/v1` 기준) | 반환 |
|---|---|---|
| GET | `/host` | Coordinator 정보: 빌드, 실행기 artifact, 상태 디렉터리 (호스트 준비 상태는 `/hosts`) |
| GET / POST | `/hosts` | 원격 호스트 목록 / SSH 정보 등록 (201) |
| POST | `/hosts/fingerprint` | 인증 전에 서버 SHA256 지문 조회 |
| POST | `/hosts/test-connection` | 호스트를 등록하지 않고 SSH 접속만 시험 |
| GET / PUT / DELETE | `/hosts/{id}` | 호스트 상세 / 인증 정보 변경 / 등록 해제 |
| POST | `/hosts/{id}/check`, `/hosts/{id}/prepare` | 점검 / 환경 준비 job(`prepare`가 실행기까지 설치) |
| POST | `/hosts/{id}/agent` | (deprecated) 실행기만 설치 job; `prepare`를 쓰세요 |
| GET | `/host-jobs`, `/host-jobs/{id}` | 원격 호스트 작업과 단계 기록 |
| GET | `/catalog` | 실제 지원 컴포넌트·프로필·제약 |
| POST | `/plans` | 노드·게스트 메모리·디스크·경고 계산 |
| GET | `/releases` | OpenStack 릴리스 카탈로그 (`arch`·`bucket` 쿼리), 최신 릴리스 순; control·compute 템플릿 보유 현황 포함 |
| POST | `/releases/plan` | 릴리스 기반 stackbox 생성 dry run: VM마다 템플릿과 clean 설치 중 어느 것을 쓸지 계산 (아무것도 만들지 않음) |
| POST | `/releases/stackboxes` | 릴리스 기반 stackbox 조립 job (`202`; 단계별 job은 `assembly`에 나열) |
| POST | `/local-host` | 로컬 호스트 등록 (`{"id":"local"}`, SSH 없음) |
| POST | `/agent-hosts` | 접속해 오는 호스트 등록 (`{"id":"edge"}`) → 1회만 표시되는 agent 토큰 |
| GET (WebSocket) | `/agent/connect` | 호스트 agent의 접속 (API token 대신 호스트 토큰) |
| GET / POST | `/stackboxes` | 목록 / 생성 job (`host_id` 필수) |
| GET / DELETE | `/stackboxes/{id}` | stackbox 상태 / 삭제 job |
| POST | `/stackboxes/{id}/deploy` | 배포 job (`{"from":"prechecks"}`로 재개) |
| POST | `/stackboxes/{id}/computes` | 배포된 다중 VM stackbox에 다음 compute VM 추가 job (`{"host_id":"dev-01"}`; 생략 시 primary 호스트, `from_snapshot`이면 compute 템플릿에서) |
| GET | `/stackboxes/{id}/instances` | stackbox의 OpenStack 인스턴스 목록 |
| GET / PUT / DELETE | `/stackboxes/{id}/instances/{server}/gpu-load` | 인스턴스의 GPU 부하 조회 / 설정 / 해제 (갱신된 부하 뷰 반환) |
| GET | `/stackboxes/{id}/gpu/guest-script` | 게스트에서 실행하는 가짜 `nvidia-smi` 스크립트 (첨부 다운로드) |
| GET | `/stackboxes/{id}/gpu/user-data` | Nova vendor data를 적용하지 않는 이미지(CirrOS)용 user-data 설치 스크립트 (첨부 다운로드, 409 가능) |
| POST | `/stackboxes/{id}/settle` | VM 재시작 뒤 서비스 응답 확인 job |
| GET | `/stackboxes/{id}/endpoints` | public 서비스 URL과 admin 인증정보 |
| PUT | `/stackboxes/{id}/nodes/{node}/faults` | 다중 VM stackbox의 VM 링크에 지연·손실 설정(`{}`이면 복구), 갱신된 stackbox |
| GET | `/stackboxes/{id}/logs?after=0&limit=1000` | 최근 로그 |
| GET | `/stackboxes/{id}/logs/stream` | 로그 SSE (`Last-Event-ID`로 재연결) |
| GET (WebSocket) | `/stackboxes/{id}/shell?rows=24&cols=80[&node=compute-1]` | VM의 root 로그인 셸 (다중 노드는 `node`로 VM 선택) |
| POST | `/stackboxes/{id}/snapshots` | VM을 스냅샷으로 얼리는 job (`node`: `control`이면 control 템플릿, `compute-N`이면 compute 템플릿) |
| GET / DELETE | `/snapshots`, `/snapshots/{id}` | 스냅샷 목록·상세 / 삭제 (다른 호스트의 복사본 포함) |
| DELETE | `/snapshots/{id}/copies/{host_id}` | 다른 호스트에 둔 스냅샷 복사본만 삭제 |
| POST | `/snapshots/{id}/export`, `/snapshots/import` | `.msb` 아카이브 내보내기 / 가져오기 (파일 또는 S3 버킷) |
| GET / POST / PUT / DELETE | `/buckets`, `/buckets/{id}` | 스냅샷을 옮길 S3 호환 버킷 등록·관리 |
| POST | `/buckets/test-connection` | 버킷을 등록하지 않고 S3 접속만 시험 |
| POST | `/buckets/{id}/check` | 등록된 버킷의 접속 점검 |
| GET / POST | `/buckets/{id}/objects`, `/buckets/{id}/index` | 버킷의 아카이브와 메타데이터 / `index.json` 다시 만들기 |
| GET / PUT / DELETE | `/relay` | relay 상태 조회 / 설정 / 해제 |
| POST | `/relay/cloudflared/update` | cloudflared 바이너리 갱신 |
| GET | `/jobs`, `/jobs/{id}` | 작업 상태·phase·오류 |
| GET | `/openapi.json` | 생성된 OpenAPI 스키마 |

Stackbox 작업 생성은 `202 Accepted`와 `Location: /api/v1/jobs/{id}`를 반환합니다.
원격 호스트 작업의 Location은 `/api/v1/host-jobs/{id}`입니다.
포탈 연동 상세는 [docs/portal-api.md](docs/portal-api.md), 정적 스키마는
[docs/openapi.json](docs/openapi.json)을 참고하세요. `stackbox openapi`로 다시 생성할 수 있습니다.

## 상태와 작업

- 상태 파일: `$XDG_DATA_HOME/stackbox/state.json` 또는 `~/.local/share/stackbox/state.json`.
  `--state-dir`로 변경할 수 있지만, **한 호스트의 운영 인스턴스는 같은 디렉터리를 사용**해야 합니다.
- 파일 잠금·임시 파일·fsync·rename으로 단일 writer와 atomic write를 보장합니다.
- 시작 시와 30초마다 msb의 `stackbox`·`stackbox.role` 라벨을 조회합니다. 기존 VM도 가져오고,
  사라지거나 정지된 VM이 있으면 `broken`으로 표시합니다. 조회 실패를 VM 삭제로 취급하지 않습니다.
- VM 생성과 배포는 각각 1개씩 실행합니다. 서로 다른 stackbox의
  생성과 배포는 병행할 수 있습니다. 같은 stackbox의 중복 작업은 `409 Conflict`입니다.
- 기본 메모리 예약 한도는 호스트 RAM의 80%입니다. `--memory-budget-mib`로 조절합니다.
  생성 요청을 받는 시점에 전체 게스트 메모리를 예약하고, 삭제 성공 후 반환합니다.
  가져온 기존 stackbox도 VM 설정의 메모리를 계산합니다.
- 배포는 호스트에서 데몬과 분리된 worker 프로세스로 실행합니다. 데몬이 종료·재시작되거나 호스트와의
  연결이 끊겨도 배포는 끝까지 진행됩니다. 다시 시작한 데몬은 실행 중이던 배포 job을 `running`으로 두고
  (`Coordinator restarted; re-attaching to the running deploy`) 받지 못한 진행 로그부터 이어 받아
  평소처럼 마무리합니다. 호스트에 10분 동안 다시 연결하지 못하면 job은 `interrupted`가 되며,
  이때 배포는 호스트에서 아직 진행 중일 수 있으니 호스트를 확인한 뒤 재시도합니다.
- 그 밖의 job과 아직 시작하지 않은(대기 중) 배포는 데몬이 갑자기 종료되면 다음 시작 시 `interrupted`가 됩니다.
  자동 재실행하지 않습니다. 배포는 저장된 phase를 확인하고 `from`을 지정해 재개합니다.
  일부 VM만 생성된 stackbox는 삭제한 뒤 다시 만듭니다.
- SIGINT/SIGTERM/SIGHUP은 새 작업 접수를 멈추고 배포를 제외한 진행 중 작업을 기다린 뒤 종료합니다.
  진행 중인 배포는 기다리지 않고 호스트에서 계속되며, 다음 시작 때 다시 붙습니다.
  두 번째 신호는 즉시 종료합니다. VM은 데몬 종료 후에도 유지됩니다.
- `owner`, `expires_at`은 metadata입니다. TTL 자동 삭제는 아직 수행하지 않습니다.
- 호스트 로그는 `logs/<stackbox>.jsonl`에 보관하며, API는 최근 1,000개 이벤트를 제공합니다.
  재연결 시 보관 범위를 벗어난 cursor는 SSE `gap` 이벤트로 알립니다. 로그 회전은 아직 없습니다.
  Kolla의 guest 로그는 기존처럼 `/var/log/stackbox/<phase>.log`에도 남습니다.

## 외부 접속

관리 API와 OpenStack 서비스는 모두 기본 `127.0.0.1`에 바인딩됩니다.
원격 포탈 backend에서 접근하려면 API token과 서비스 바인딩을 지정합니다.

```bash
export STACKBOX_API_TOKEN='<운영용-token>'
stackbox serve --bind 0.0.0.0:10010 --public-bind 0.0.0.0
# POST /stackboxes에서 external_host를 이 호스트의 DNS 이름/IPv4로 지정
# 이후 API 호출에 Authorization: Bearer <운영용-token> 헤더 추가
```

loopback 이외의 관리 API 바인딩에는 token이 필수입니다. TLS는 사설망의 reverse proxy에서
종료하도록 구성하세요. 포탈 backend가 stackboxd와 OpenStack API를 호출하고, 브라우저에 admin
인증정보를 전달하지 않는 구조를 전제로 합니다. CORS는 활성화하지 않습니다.

stackbox별 public TCP block은 `15000 + 20 × n`으로 20개 포트씩 예약하고(`PUBLIC_PORT_BLOCK`),
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

6~8번은 모든 stackbox에 설치되는 데이터 스토어입니다. 배포가 끝나면 `GET /stackboxes/{id}/endpoints`의
`datastores`와 포탈의 **Data stores** 패널에서 계정·비밀번호·데이터베이스 목록을 확인합니다.
이 릴리스 전에 만든 stackbox는 이 포트를 공개하지 않으므로, 필요하면 stackbox를 다시 만들거나
게스트의 `/etc/stackbox/identity.conf`와 `msb modify --port`로 직접 추가합니다.

서비스 카탈로그의 public URL에는 `external_host`와 호스트 포트를 기록하고,
내부 통신은 VM의 고정 서비스 주소(`10.254.0.2`)와 기존 서비스 포트를 사용합니다.
외부 호스트 변경이나 포트 재할당은 현재 지원하지 않습니다.

관리 계층 추가 전에 생성된 VM은 원래 포트 설정으로 가져오므로 `exposure: null`입니다.
기존 다중 VM Stackbox는 조회·인증정보 조회·삭제를 유지하지만 새 배포 엔진으로 재배포할 수 없습니다.
자동 변환·자동 삭제하지 않으므로 필요한 데이터를 보관한 뒤 새 Stackbox를 만드세요.

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
  stackbox는 VM을 `nested_virt(true)`로 만들고, 호스트가 nesting을 지원하지 않으면 nesting 없이 다시 만듭니다.
  콘솔 프록시는 켜지 않습니다.
- 보안 그룹은 hybrid iptables 대신 `openvswitch` firewall driver를 사용합니다.
- L3 agent는 address scope용 CONNMARK 규칙만 빼고 실행합니다(`src/kolla/neutron_sitecustomize.py`).
  stackbox는 address scope를 설정하지 않습니다.
- 외부망 `public`(172.24.4.0/24, flat `physnet1`)은 VM의 veth `stackbox-ext`/`stackbox-extgw`
  뒤에 있습니다. 해당 VM이 172.24.4.1로 응답하고 eth0으로 masquerade합니다.
  `stackbox-external.service`가 재부팅 때마다 다시 구성합니다.

게스트는 남는 램을 페이지 캐시로 채우므로 실제 사용량은 할당량의 합까지 늘어날 수 있습니다.
디스크 계획에는 root disk와
Docker/containerd용 ext4 디스크 두 개가 포함됩니다. sparse disk의 현재 사용량과는 다릅니다.

### stackbox 게스트 커널(VFIO/mdev)

msb의 기본 게스트 커널에는 모듈 로딩이 없고 VFIO·mdev도 들어 있지 않습니다. stackbox는 커널 설정
조각(`scripts/libkrunfw/fragments/`)을 얹은 libkrunfw를 빌드해 **기본 게스트 커널**로 씁니다.
`vfio-mtty` 조각이 켜는 VFIO·VFIO_MDEV·VFIO_PCI·SAMPLES와 커널 샘플 `mtty` 드라이버로 게스트
안에 `mtty-1` mdev 타입이 생기므로, Nova의 vGPU 경로를 실제 GPU 없이 시험할 수 있습니다.
`neutron-netfilter` 조각은 Neutron L3 agent가 라우터마다 쓰는 CONNMARK(연결 mark 저장·복원)를
켜서, 이 커널에서도 테넌트 라우터와 플로팅 IP가 동작하게 합니다.
새 기능이 필요하면 조각 파일을 하나 더 넣으면 되고 빌드 스크립트는 고치지 않습니다.

```bash
task dev                          # 커널 빌드(실패해도 계속) + 데몬 + 포탈
stackbox host local                   # 이 머신 준비: msb, stackbox 커널 설치, 검증 VM
stackbox host list                    # local Local Ready · custom kernel …/libkrunfw/<id>/… · kernel 6.12.113 · VFIO · mdev · mtty-1×24

task libkrunfw                    # 이 머신용 커널만 빌드: target/libkrunfw/<arch>/
task libkrunfw:all                # aarch64와 x86_64 (호스트와 다른 쪽은 크로스 컴파일)
sh scripts/build-libkrunfw.sh --list-fragments   # 조각 목록과 각 조각의 목적
```

호스트 준비는 그 호스트의 `~/.local/share/stackbox/libkrunfw/<id>/`에 커널을 설치합니다(`id`는
manifest의 sha256 앞 16자). coordinator와 각 호스트의 stackbox-agent는 시작할 때 이 설치를 찾아
microsandbox SDK에 msb와 라이브러리 짝을 넣습니다. `~/.microsandbox`는 건드리지 않습니다.
SSH 호스트도 준비 단계에서 Linux `.so`를 같은 방식으로 업로드하고 sha256을 확인합니다.
manifest에 적힌 `msb`가 호스트의 msb와 다르면 stock 커널로 부팅하고 그 이유를 호스트 정보의
`custom_kernel_reason`과 `stackbox host list`에 남깁니다.

조각은 커널 `.config` 문법 그대로(`=y`, 문자열·숫자 값, `# CONFIG_... is not set`) 쓰고,
아키텍처 전용 설정은 `<name>.<arch>.config`에 둡니다. 빌드는 `olddefconfig` 뒤에 모든 조각의 모든
줄이 최종 `.config`에 남았는지 확인하고, 하나라도 바뀌면 어떤 조각의 어떤 줄이 무엇으로 바뀌었는지
보여 주며 실패합니다. 두 조각이 같은 심볼에 다른 값을 적으면 빌드 전에 멈춥니다.

`STACKBOX_LIBKRUNFW_PATH`는 최우선 override입니다. 저장소 밖에서 빌드한 커널을 한 프로세스에서만
시험할 때 쓰고, `STACKBOX_LIBKRUNFW_DIR`은 빌드 산출물 디렉터리를 바꿉니다. `task libkrunfw:path`는
이 머신이 빌드한 커널 경로를 출력합니다. Docker가 없으면 커널 빌드를 건너뛰고 stackbox는 stock
커널로 동작하며(mtty 불가), 그 사실이 호스트 정보에 나옵니다. 릴리스 바이너리가 커널을 버킷에서
받아 오는 배포 훅은 아직 없습니다([docs/libkrunfw.md](docs/libkrunfw.md)의 TODO).

### GPU(가상 장치)

stackbox에는 실제 GPU가 없습니다. 그래도 Nova가 GPU를 스케줄링하는 경로를 시험할 수 있도록 가상 장치
두 가지를 제공합니다. `simulated`는 일반적인 vGPU 배포와 같은 모양으로 compute마다 표준 `VGPU`
인벤토리를 child resource provider에 광고하고(OpenStack 2025.1 이상, 게스트 장치 없음),
`mtty`는 stackbox 커널의 `mtty` mdev를 `VGPU`로 붙입니다(2025.1 이상). 둘 다 연산하지 않습니다.
`mtty`는 업스트림 Nova가 `mtty`를 아는 2026.2부터는 Nova 코드를 그대로 쓰고, 2025.1·2025.2·
2026.1은 stackbox가 배포 때 Nova에 넣는 백포트로 같은 경로를 씁니다(docs/gpu.md).

```bash
# 계약을 직접 정할 수도 있습니다: 가짜 GPU 장치 수와 flavor가 요구할 CUSTOM_* trait.
stackbox plan --components nova --gpu simulated --gpu-count 4 --gpu-devices 2 --gpu-trait CUSTOM_MAAS
```


```bash
stackbox plan --components nova --gpu simulated --gpu-count 2
stackbox create demo-gpu --host-id local --components nova --gpu simulated --gpu-count 2
stackbox deploy demo-gpu

# mtty는 stackbox 커널이 준비된 호스트에서 바로 됩니다(위 `stackbox host local`).
stackbox create demo-vgpu --host-id local --components nova --gpu mtty
stackbox deploy demo-vgpu
```

GPU stackbox의 인스턴스에는 실제 GPU 없이 `nvidia-smi`를 흉내 내는 게스트 스크립트가 설치되고,
포탈의 **GPU 부하** 탭에서 프로파일·패턴·주기·메모리 비율을 조절할 수 있습니다. Telegraf
`inputs.nvidia_smi`나 `nvidia-smi --query-gpu=... --format=csv` 같은 모니터링 경로를 그대로
시험할 수 있습니다. 게스트 설치는 cloud-init 이미지에 vendordata로 자동으로 되고, CirrOS는
user-data 설치 스크립트를 씁니다. 지원 명령·메타데이터 형식·반영 지연·한계는
[docs/gpu.md](docs/gpu.md#가짜-nvidia-smi와-부하-제어)에 정리했습니다.

GPU stackbox는 템플릿(스냅샷) 대신 클린 설치하고, GPU stackbox의 control VM은 control 템플릿으로 얼릴 수
없습니다. 릴리스별 조건·배포가 쓰는 설정·한계는 [docs/gpu.md](docs/gpu.md)에 정리했습니다.

## 코드와 검증

`src/lib.rs`가 라이브러리 진입점입니다. `StackboxManager`가 계획·상태·예약·작업을 관리하고,
`src/manager/backend.rs`가 microsandbox VM 로직을 연결합니다. `src/api.rs`와 `src/main.rs`는
같은 manager를 사용합니다. 새 VM 이름은 stackbox id와 같고(`<stackbox>`), 이전 버전이 만든 `<stackbox>-all-in-one` VM도 그대로 인식합니다.

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

관리/API 테스트는 fake backend로 작업 실패·재개·배포 동시성·재시작 복구와 배포 재연결·메모리 예약·
파일 잠금·저장 실패·인증·SSE를 검증하며 VM을 띄우지 않습니다.

stackbox VM 사이의 L2 링크(`src/link/`, [docs/poc-l2-link.md](docs/poc-l2-link.md))는 실제 VM 3대로 확인합니다.
게스트용 Linux stackbox-agent가 필요합니다.

```bash
sh scripts/build-agent.sh aarch64
STACKBOX_GUEST_AGENT=$PWD/dist/agent/aarch64/stackbox-agent \
  cargo test --release --test stackbox_link -- --ignored --nocapture
```

실제 OpenStack 설치는 별도의 로컬 Stackbox로 확인합니다. 기본 구성은 VM 1대에
3 GiB를 할당하며, 최초 실행에는 패키지와 컨테이너 이미지 다운로드가 필요합니다.
기존 daemon과 다른 상태 디렉터리를 사용하세요.

```bash
cargo build --bin stackbox
state_dir=$(mktemp -d /tmp/stackbox-openstack.XXXXXX)
target/debug/stackbox --state-dir "$state_dir" create e2e
# 선택 서비스까지 확인하려면: create e2e --components placement,horizon
target/debug/stackbox --state-dir "$state_dir" deploy e2e
python3 scripts/verify-openstack.py e2e --state-dir "$state_dir"
# 로컬 VM 1대를 재시작하고 토큰·업로드한 이미지의 영속성까지 확인하려면:
python3 scripts/verify-openstack.py e2e --state-dir "$state_dir" --restart-local-vms
target/debug/stackbox --state-dir "$state_dir" delete e2e
```

배포의 `verify` 단계는 VM 내부에서 토큰 발급과 Glance 이미지 업로드·다운로드 내용
비교·삭제를 검사합니다. Placement는 인증된 resource provider 조회, Horizon은 브라우저와 같은
CSRF 로그인과 로그인 후 대시보드 응답까지 확인합니다. Nova가 없는 stackbox의 Horizon은 로그인 후
Compute 개요(403) 대신 이미지 목록으로 이동하도록 설정합니다. Neutron은 4개 agent 상태를 확인하고,
임시 tenant network·subnet·router를 만들어 동일 VM에서 라우터 게이트웨이를, 라우터
namespace에서 DHCP 포트를 ping한 뒤 지웁니다. `scripts/verify-openstack.py`는 호스트에서 공개 포트로 접속해
인증·서비스 카탈로그·프로젝트 조회·인증 없는 요청 거부·1 MiB 이미지 왕복과 삭제 후
404 응답을 검사합니다. 인증정보는 CLI에서 메모리로만 받아 사용하며 출력하거나 파일에
저장하지 않습니다. 실패 시 배포 로그를 확인한 뒤 `deploy e2e --from <phase>`로 재개할 수 있습니다.

Kolla의 기본 Ansible 출력에는 서비스 비밀값이 포함될 수 있어 Stackbox는 작업명과
호스트별 결과만 전달하는 callback을 사용합니다. 실패 상세는 control VM의
`/var/log/stackbox/ansible-failures.jsonl`에 root 전용 권한(`0600`)으로 남기며,
Ansible의 `no_log` 작업은 이 파일에서도 내용을 숨깁니다.

이전 단일 VM 전환의 구조와 호환성은 [docs/single-vm.md](docs/single-vm.md), 그 이전 다중 VM 실배포
기록은 [docs/local-validation.md](docs/local-validation.md)에 있습니다.

## 릴리스

작업은 `dev`(기본 브랜치)에서 하고, `main`에 push될 때마다
[`.github/workflows/release.yml`](.github/workflows/release.yml)이 GitHub Release를 만듭니다.

- 기본은 patch입니다. 직전 태그 이후 커밋 메시지에 `#minor`/`[minor]`나 `#major`/`[major]`가
  있으면 그만큼 올립니다. Actions의 수동 실행(`workflow_dispatch`)에서도 bump를 고를 수 있습니다.
- `scripts/set-version.sh`로 `Cargo.toml`·`Cargo.lock`·`web/package.json`·`web/package-lock.json`의
  버전을 바꿔 `Release vX.Y.Z` 커밋을 `main`에 올리고, `vX.Y.Z` 태그와 GitHub Release(직전 태그 이후
  커밋 목록)를 만든 뒤 `main`을 `dev`에 다시 merge합니다.
- `Cargo.toml`의 버전에 아직 태그가 없으면(첫 릴리스나 `dev`에서 직접 올린 경우) 올리지 않고 그 버전으로
  릴리스합니다.
- 버전 커밋은 `Cargo.toml`을 바꾸므로 릴리스마다 `STACKBOX_BUILD_ID`도 바뀝니다. 새 릴리스를 배포하면
  원격 호스트의 agent를 다시 설치해야 합니다.
- 릴리스 커밋에서 네 플랫폼(linux x86_64·aarch64, macOS aarch64, Windows x86_64)의 stackbox-agent를
  빌드합니다. 네 빌드의 build id가 모두 같아야 하며, 다르면 아무것도 push하지 않습니다. 성공하면 GitHub Release
  `vX.Y.Z`에 바이너리 4개와 플랫폼별 sha256·크기를 담은 `manifest.json`을 함께 올립니다. 릴리스 노트에는
  build id와 sha256 표가 들어갑니다. 같은 태그로 빌드한 coordinator는 이 agent를 그대로 받아들입니다.
- 같은 릴리스 이미지를 GHCR `ghcr.io/preinpost/stackbox`(`vX.Y.Z`, `latest`, `sha-<short>`)에도 올립니다.
- coordinator는 `stackbox serve`와 `stackbox host prepare`를 시작할 때 `STACKBOX_AGENT_URL`(기본
  `https://github.com/preinpost/stackbox/releases/download`)에서 받은 릴리스의 `manifest.json`을 읽고, `dist/agent`에 없는
  플랫폼의 실행기를 호스트에 처음 설치할 때 받아 sha256과 build id를 확인한 뒤 `<state-dir>/agents/<sha256>/`에
  보관합니다. 릴리스되지 않은 빌드는 manifest가 없어 건너뜁니다. `STACKBOX_AGENT_URL=`(빈 값)이면 끕니다.
- macOS와 Windows agent는 `docker/agent-cross.Dockerfile`(cargo-zigbuild, zig 0.16)로 Linux에서 cross 빌드합니다.
  macOS 링크에는 MacOSX.sdk의 `.tbd` 파일이 필요해 로컬에서는 Xcode SDK를 쓰고(`MACOS_SDK`로 바꿀 수 있음),
  CI의 macOS 빌드 잡(`macos-latest`)은 러너에 설치된 Xcode SDK(`xcrun`)를 씁니다. 시크릿: `RELEASE_TOKEN`(선택, 없으면 `GITHUB_TOKEN`을 씀)으로
  릴리스와 `main`·`dev` push를 하고, GHCR 푸시는 `GITHUB_TOKEN`(`packages: write`)을 씁니다.
