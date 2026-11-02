# 원격 호스트 등록과 준비

등록한 단일 원격 호스트에서 Farm 생성·조회·Kolla 배포·삭제를 실행할 수 있다.
Farm마다 VM 1대를 실행하고 모든 선택 서비스를 같은 VM에 설치한다.
현재 배치 단위는 [Farm 전체](remote-farms.md)이며 역할별 호스트 배치는 사용하지 않는다.

## 포탈 사용

1. farmd와 웹포탈을 실행하고 **Hosts → 호스트 등록**으로 이동한다.
2. 서버 IP/호스트명과 SSH port를 입력해 서버 지문을 조회한다. 이 단계는 인증하지 않는다.
3. 서버 콘솔 등 독립적인 신뢰 경로에서 확인한 SHA256 지문과 비교한다.
   예를 들어 서버 콘솔에서 `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`로 확인할 수 있다.
   조회 결과만 보고 신원을 신뢰하지 않는다. 일치 확인 체크 후 등록한다.
4. 호스트 ID, SSH 사용자, 비밀번호 또는 SSH 개인 키와 필요 시 키 암호를 입력한다.
   **접속 테스트**는 인증 정보를 저장하지 않고 SSH 로그인과 `uname`만 확인한다.
   등록 후 SSH 접속과 OS·아키텍처·KVM·메모리·디스크·sudo·msb·Docker·이미지를 점검한다.
5. **환경 준비**에서 실제 설치 작업을 시작한다. 진행 단계와 오류는 Hosts/Jobs에서 확인한다.
6. **Farm 실행기 설치** 후 호스트 상세의 **이 호스트에 Farm 만들기**로 Farm을 생성한다.
   준비가 끝나지 않았거나 마지막 SSH 점검이 5분을 넘은 호스트는 선택할 수 없다.

접속할 때마다 등록한 SHA256 지문을 대조하며 불일치 시 인증 정보를 보내기 전에 중단한다.
지문 변경을 자동으로 수락하지 않는다. 신뢰 경로에서 변경을 확인한 뒤 `인증 변경`에서 지문과 인증 정보를 교체한다.
호스트 ID·주소·port·사용자는 유지하며, 작업 실행 중에는 교체를 거절한다. 이 기능은 Farm이 있는 호스트도 등록 해제 없이 인증 정보를 복구할 수 있게 한다.
현재 SSH 인증은 password 또는 개인 키 방식이다. SSH agent forwarding, bastion/ProxyJump,
keyboard-interactive/MFA, 호스트 인증서/CA, sudo 비밀번호 입력은 지원하지 않는다.

## 타겟 조건과 준비 작업

- Linux x86_64 또는 aarch64. Coordinator와 다른 아키텍처여도 된다. 실행기는 대상 아키텍처용 바이너리를 따로 둔다.
- SSH 사용자가 `/dev/kvm`을 읽고 쓸 수 있어야 한다. 클라우드 VM이면 nested virtualization이 필요할 수 있다.
  설치 작업으로 KVM을 활성화하거나 접근 권한을 임의 변경하지 않는다.
- 비대화형 SSH exec, Bash, GNU `timeout`, `awk`, `grep`, `df`, `flock`, `tar`, `install`, `sha256sum`, `curl`이 필요하다.
- 시험 VM 준비에는 가용 메모리 768 MiB와 홈 디렉터리 볼륨의 여유 디스크 8 GiB 이상을 요구한다.
  실제 Farm은 별도로 기본 3 GiB에 선택 서비스 메모리를 더한 게스트 메모리를 예약하며, 원격 호스트별 한도는 전체 메모리의 80%다.
- Docker는 필요 없다. OS 패키지와 sudo 정책은 자동 설치·변경하지 않는다.
- 타겟에서 GitHub 릴리스, OCI 레지스트리(Docker Hub의 `rockylinux/rockylinux:10`)와 배포판 패키지
  미러로 나갈 수 있어야 한다.

준비 작업은 다음 순서다.

1. 등록한 지문으로 SSH 인증 후 조건을 다시 점검한다.
2. msb가 없거나 0.7.13보다 낮으면 공식 `v0.7.13` Linux 릴리스 번들과 `checksums.sha256`을 HTTPS로 내려받고,
   정확한 번들 항목의 SHA256을 확인한 후 msb와 libkrunfw를 사용자 홈의 `~/.microsandbox/bin`,
   `~/.microsandbox/lib`에 설치한다. 이후 명령은 해당 bin 경로를 사용한다.
   이미 0.7.13이면 재사용한다. 낮은 버전은 msb 공식 설치기처럼 그 자리에서 교체하며,
   실행 중인 sandbox는 기존 런타임으로 계속 돈다. 더 높은 버전은 다운그레이드하지 않고 실패한다.
3. golden image는 빌드하지 않는다. 그 호스트의 첫 farm이 msb로 빌드한다(README의 Golden image 참고).
   점검 결과의 `golden_image_available`은 이 빌드의 golden image가 이미 있는지만 알린다.
4. UUID를 포함한 `farm-check-*` 시험 VM을 golden image의 기본 이미지로 512 MiB 부팅하고 `msb exec ... /bin/true`를 실행한 뒤 삭제한다.
   외부 네트워크는 비활성화하고 최대 실행 시간을 120초로 제한한다.
5. 최종 점검과 시험 VM 성공 후에만 호스트를 `ready`로 표시한다. `FARM_AGENT_PATH`가 설정되어 있으면 실행기 업로드·체크섬·프로토콜 검증도 수행한다.

`runtime_ready`는 Linux/KVM/msb 조건을 확인한 상태다. `ready`는 이미지와 시험 VM 검증까지 통과한 상태다.
읽기 전용 점검은 같은 runtime에서 앞서 성공한 시험 VM 검증 결과를 유지한다.
준비 작업을 다시 실행하면 새 시험 VM 검증이 필요하다. 런타임·이미지는 이미 준비된 경우 재사용한다.
호스트별로 작업 하나만 허용하며 중앙에서 최대 두 호스트 작업을 동시에 실행한다.
설치/이미지 작업은 사용자 홈의 `~/.local/share/farm/bootstrap.lock`도 사용한다.

설치에는 10분, 이미지 빌드에는 20분, VM 검증에는 3분의 원격 실행 제한을 둔다.
실패 시 현재 단계와 조건을 확인한 후 명시적으로 재실행한다. 롤백은 하지 않는다.
데몬은 정상 종료 시 작업을 기다린다. 강제 종료 후에는 진행 중 작업을 `interrupted`로 바꾸고 자동 재실행하지 않는다.
원격 명령은 제한 시간까지 남을 수 있으므로 호스트 작업/잠금과 `farm-check-*` VM을 확인한 뒤 재시도한다.
시험 VM 삭제가 실패하면 작업도 실패하며 원격 호스트에서 해당 VM을 확인한다.

## 인증 정보 보관과 복구

`state.json`에는 호스트 메타데이터, 임의 `credential_ref`, XChaCha20-Poly1305 nonce/ciphertext만 기록한다.
호스트 ID·주소·port·사용자·지문과 credential_ref를 추가 인증 데이터로 묶어 다른 호스트로 ciphertext를 옮겨도 복호화되지 않는다.
인증 정보 원문, SSH/명령 stdout/stderr는 작업 기록에 저장하지 않는다. 원격 출력은 제한된 버퍼에서 점검 필드만 파싱한다.
비밀번호·개인 키를 CLI 인자나 개인 키 임시 파일로 전달하지 않고 Rust SSH 클라이언트로 인증한다.

마스터 키는 상태 디렉터리의 `credentials.key`에 32 raw bytes로 한 번 생성하며 파일 권한은 0600이다.
상태 디렉터리는 0700이다. key는 API로 노출하지 않는다. **백업에는 원래 `credentials.key`와 `state.json`이 모두 필요하다.**
호스트 기록이 있는데 key가 없거나 맞지 않으면 시작을 거절한다. 임의로 새 key를 생성해 기존 인증 정보를 대체하지 않는다.
key를 가진 동일 OS 사용자는 복호화할 수 있다. 이 저장소는 OS 계정 접근 제어나 외부 KMS를 대신하지 않는다.

브라우저의 등록 요청은 Query/Mutation cache와 local/session storage에 인증 정보를 저장하지 않는다.
요청 완료/실패 시 비밀번호·키·키 암호 입력을 비우며 호스트 GET 응답에는 `auth_method`만 포함한다.
포탈과 API는 신뢰한 운영자용이다. 원격에서 포탈을 사용할 때는 기존 지침대로 TLS와 인증을 제공하는 reverse proxy 뒤에 둔다.

등록 해제는 중앙 호스트·작업 기록과 암호화된 인증 정보만 제거한다. msb, 이미지, Docker 캐시, 원격 VM은 남는다.
원격 설치 제거는 후속 작업이다. 인증 변경은 같은 SSH endpoint에서 비밀번호/키/passphrase와 검증한 서버 지문을 교체한다.
Farm이 남아 있는 호스트의 등록 해제는 거절한다. 먼저 해당 Farm들을 삭제해야 한다.

## API

모든 경로는 `/api/v1` 아래이며 기존 bearer 인증 정책을 따른다. OpenAPI와 포탈 타입도 함께 생성한다.

| Method | Path | 결과 |
|---|---|---|
| POST | `/hosts/fingerprint` | `{address, port}` → `{fingerprint, algorithm}`; 인증 전 조회 |
| GET / POST | `/hosts` | 목록 / `RegisterHost` 저장, 201 + Location |
| GET / PUT / DELETE | `/hosts/{id}` | 상세 / 인증 변경 / 등록 해제; 작업 실행 중이면 409 |
| POST | `/hosts/{id}/agent` | 실행기 업로드·SHA256·프로토콜 확인; 202 + HostJob |
| POST | `/hosts/{id}/check` | 읽기 전용 SSH 점검; 202 + HostJob |
| POST | `/hosts/{id}/prepare` | 조건 점검·설치·이미지·시험 VM; 202 + HostJob |
| GET | `/host-jobs`, `/host-jobs/{id}` | 상태·단계·실패 이유 |

호스트 작업의 Location은 `/api/v1/host-jobs/{id}`다. 같은 호스트의 중복 작업은 409로 거절한다.
비밀 입력은 `credentials: {method: "password", password: ...}` 또는
`{method: "private_key", private_key: ..., passphrase: ...}` 형태다. 조회용 타입과 분리되어 있다.
공용 임의 명령 실행 API는 제공하지 않는다.

## 검증 범위

- 자동 테스트: loopback SSH 서버와 실제 SSH 프로토콜로 password·암호화 키 인증, 무인증 지문 조회,
  지문 불일치 시 인증 전 차단, 준비 단계 연결/재실행/실패, 작업 충돌, 암호화 저장/무결성/다른 key 거절,
  재시작 복구, HTTP 상태·Location·입력 검증, 조회 응답의 비밀 비노출을 확인한다.
  이 서버는 설치 명령의 결과를 모의하므로 실제 KVM 부팅 검증을 대체하지 않는다.
- 포탈 테스트: 지문 확인 전 등록 차단, 주소 변경 시 지문 확인 초기화, 오류 후 비밀 입력 삭제,
  Query cache 비밀 비저장, 프록시 경로·요청 전달과 기존 SSE/정적 서버 동작을 확인한다.
- 2026-10-20 수동 확인: 임시 Rocky Linux aarch64 VM의 실제 OpenSSH 서버에 password/키로 접속,
  OS/자원 점검, 실제 서버 지문 변경 차단, 공식 msb 0.7.8 번들의 체크섬·설치·재실행,
  데스크톱/390px 포탈 등록·작업 표시·KVM 미지원 사전 차단을 확인했다.
  시험 VM 내부에 `/dev/kvm`이 없어 **원격 golden image 빌드부터 KVM 시험 VM 부팅까지 전체 성공은 미검증**이다.
  실제 KVM 호스트를 제공하면 이 경로를 추가 확인해야 한다.

참고: [microsandbox Quickstart](https://docs.microsandbox.dev/getting-started/quickstart),
[공식 v0.7.13 릴리스](https://github.com/superradcompany/microsandbox/releases/tag/v0.7.13),
[russh](https://github.com/warp-tech/russh).


## 단일 원격 Farm 실행

Coordinator는 `dist/agent/<target>/` 디렉터리마다 `farm-agent`(Windows는 `farm-agent.exe`)를 읽고, 실행 파일
헤더(ELF·Mach-O·PE)로 플랫폼(`linux-x86_64`, `linux-aarch64`, `macos-aarch64`, `windows-x86_64` 등)을 정한다.
`FARM_AGENT_PATH`로 파일을 하나 더 지정할 수 있다. 같은 플랫폼에 서로 다른 binary가 있으면 시작하지 않는다.
다음 스크립트는 공식 Rust 1.99.1 Bookworm 이미지와 잠금 파일로 실행기를 빌드한다. Linux 실행기는 glibc 2.36+
환경(Ubuntu 24.04/Rocky 10 등)용이고, macOS·Windows 실행기는 cargo-zigbuild로 cross 빌드한다.
첫 빌드에는 Docker/BuildKit과 인터넷이 필요하다.

```bash
sh scripts/build-agent.sh          # 현재 아키텍처의 Linux 실행기; aarch64/x86_64 인자로 명시 가능
sh scripts/build-agent.sh macos-aarch64   # MACOS_SDK 또는 Xcode SDK 필요
sh scripts/build-agent.sh windows-x86_64
cargo run -- serve
```

실행기는 daemon 시작 시 로드하며 형식·아키텍처·크기를 검사한다. 변경 시 daemon을 재시작한다.
로드한 실행기가 없는 플랫폼은 `FARM_AGENT_URL`(기본 `https://public-bucket.preinpost.in/farm-agent`)에 이 빌드의
`<build id>/manifest.json`이 있으면 그 실행기를 쓴다. 호스트에 처음 설치할 때 받아 크기·sha256·플랫폼·build id를
확인하고 `<state-dir>/agents/<sha256>/`에 보관한다. 릴리스 태그로 빌드한 coordinator만 해당하며, 빈 값이면 끈다.
SSH 설치는 아직 Linux 호스트만 지원한다.
`task dev`는 coordinator를 빌드한 뒤 `task agent-build`(기본 `ARCHS="x86_64 aarch64"`, 소스가 그대로면 건너뜀)로 실행기를
함께 빌드한다. 빌드한 coordinator의 build id로 릴리스된 manifest가 있으면(`scripts/agents-published.sh`) 실행기는 R2에서
받으므로 빌드하지 않는다.
`farm serve`는 시작할 때, 다른 빌드의 실행기가 설치돼 있고 작업 중이 아닌 SSH 호스트마다 현재 빌드의 실행기가
로드돼 있으면 `install_agent` host job을 시작한다. 그래서 coordinator를 다시 빌드해도 호스트를 손으로 재설치할 필요가 없다.
`GET /host`의 `agent_artifacts`는 로드했거나 받을 수 있는 실행기마다 SHA256·OS·아키텍처·프로토콜·`supports_single_vm` 기능 정보를 반환한다. 경로와 binary는 노출하지 않는다.
Hosts의 `환경 준비`는 설정된 실행기가 있으면 함께 설치하며, `Farm 실행기 설치` 버튼으로 따로 설치할 수도 있다.
실행기 설치 자체는 KVM이 없어도 버전 확인까지 가능하지만 Farm 생성에는 KVM·이미지·시험 VM 준비 성공이 필요하다.

실행기는 SSH로 `~/.local/share/farm/agents/<sha256>/farm-agent`에 전송한다.
임시 파일의 checksum을 확인한 뒤 0700으로 atomic rename하며 같은 digest는 checksum 확인 후 재사용한다.
실행마다 SSH host key와 인증을 확인하고, JSON 요청을 stdin으로 보내고 JSON line의 phase/output/result를 받는다.
포트를 열거나 상주 프로세스를 설치하지 않는다. 실행기는 SSH 비밀번호·개인 키를 받거나 보관하지 않는다.

Farm 생성 요청에는 `host_id`가 필요하다. Coordinator가 실행 중인 머신도 `POST /local-host`로 등록한
`kind: "local"` 호스트이며, 같은 farm-agent 프로토콜을 SSH 대신 하위 프로세스(`farm agent`)로 주고받는다.
로컬 호스트는 인증정보·실행기 설치가 없고, 환경 준비는 msb·golden image 확인과 시험 VM 부팅만 한다.

```json
{"id":"remote-demo","host_id":"worker-01","external_host":"worker-01.example"}
```

포탈에서도 실행 호스트를 고르면 주소를 공개 URL 기본값에 채운다. 사용자에게 접근 가능한 IPv4/DNS로 조정할 수 있다.
`external_host=localhost`는 원격 서비스에 맞지 않으므로 명시적으로 설정한다.
생성하려는 타겟은 `ready`, 설치된 agent, 최근 5분 이내 점검 결과가 있어야 한다.
VM 1대와 선택한 모든 서비스는 같은 원격 서버에서 실행하며 VM 간 overlay는 필요하지 않다.
원격 public 서비스는 0.0.0.0의 `15000 + 20n` block 중 선택한 서비스 offset(Keystone 0, Glance 1, Placement 2, Horizon 3, Neutron 4)을 사용한다. 방화벽과 외부 주소의 경로는 운영자가 설정한다.
동일 port block을 다른 물리 호스트에서 재사용할 수 있으며 실제 타겟에서 사용 중인 포트도 생성 직전에 확인한다.

기존 deploy/delete/endpoints/logs API와 CLI는 Farm의 `host_id`에 따라 원격으로 전달한다.
배포 단계·출력은 중앙의 기존 log/SSE를 통해 전달한다. OpenStack credentials는 명시적 endpoints 요청에서만 반환하고 저장/로그에 쓰지 않는다.
포탈 프록시는 기존처럼 credentials를 제거한다.

중앙 `state.json`에는 안정적인 `coordinator_id` UUID를 추가한다. 호스트 등록 이전에 만든 `host_id: null` Farm은
실행할 agent가 없으므로 삭제(기록 제거)만 할 수 있다.
각 원격 Farm은 `remote_generation` UUID와 설치된 `agent_sha256`을 고정한다.
호스트의 실행기를 갱신해도 기존 Farm이 사용 중인 digest 디렉터리는 지우지 않는다.
상태 파일·암호화 key와 이 Coordinator UUID를 함께 복구해야 이전 원격 Farm을 계속 관리할 수 있다.

타겟은 `~/.local/share/farm/worker-v1/registry.json`에 소유 Coordinator·generation·계획·예약·공개 포트를 기록한다.
VM 생성 전에 atomic 저장하고 host/farm 작업 파일 잠금으로 다른 worker와 충돌을 막는다.
worker도 전체 예약과 물리 가용 메모리·KVM을 확인한다. 같은 generation의 완료된 create는 기존 VM을 반환하며,
불완전한 create는 자동 재생하지 않는다. 삭제한 generation의 오래된 create는 재생할 수 없다.
다른 Coordinator/generation의 VM과 registry에 없는 외부 VM은 임의로 채택하거나 삭제하지 않는다.
소유권 충돌이나 registry 손실은 운영자가 원격 상태를 확인해 복구해야 한다.

원격 조회는 해당 Coordinator의 Farm만 반환한다. SSH/agent 조회 실패 시 Farm 상태·VM 목록·예약을 유지하고
`observation_error`와 `last_seen_at`으로 마지막 관측임을 표시한다. 실패를 빈 inventory로 취급해 예약을 풀지 않는다.
연결이 회복되면 다시 관측한다. 배포/생성은 중앙에서 각각 하나씩, 타겟에서도 충돌을 검사한다.
강제 종료한 작업은 interrupted가 되며 자동 재실행하지 않는다. SSH 연결이 끊겨도 원격 작업은 timeout까지 남을 수 있다.
실행기는 create 10분, deploy 2시간, delete 3분으로 제한하며 agent의 파일 잠금과 registry를 확인한 뒤 재시도한다.

배포만은 요청과 분리해 실행한다. Coordinator는 배포 job마다 원격 job ID(UUID)를 만들어 job 기록(`remote_job_id`)에
먼저 저장하고 deploy 요청에 담아 보낸다. 실행기는 `~/.local/share/farm/worker-v1/jobs/<job ID>/`를 만들고
자기 자신을 새 세션의 worker 프로세스(`farm-agent agent worker <dir>`, 로컬은 `farm agent worker <dir>`)로 띄운 뒤
그 디렉터리를 따라 읽어 진행을 전달한다. worker는 `deploy.lock`과 Farm 잠금을 잡고 진행 frame을 `frames.jsonl`에,
결과를 `result.json`(atomic rename)에 쓰며, 실행 중에는 `worker.lock`을 쥐고 `pid`를 남긴다. stderr는 `worker.log`에 남는다.
SSH 채널·`timeout`·하위 프로세스 종료·터미널 신호로는 요청을 처리하던 쪽만 끝나고 worker는 2시간 제한까지 계속한다.
같은 job ID의 deploy 요청은 새로 배포하지 않고 기존 job에 붙는다. `attach` 요청(`job_id`, 이미 받은 frame 수 `offset`)은
남은 frame부터 결과까지 전달한다. job을 시작한 Coordinator만 붙을 수 있고, worker가 결과 없이 사라졌으면
`deploy worker exited without a result` 오류를 돌려준다. 끝난 job 디렉터리는 7일 뒤 새 배포를 시작할 때 지운다.

Coordinator는 적용한 frame 수를 job 기록(`remote_frames`)에 저장한다(step/phase마다, 출력은 2초마다).
연결이 끊기거나 Coordinator가 재시작하면 그 수부터 `attach`해 같은 job을 평소처럼 마무리한다. 재시작 직전 2초 안의
출력 몇 줄은 로그에 두 번 남을 수 있다. 호스트에 10분 동안 다시 붙지 못하면 job은 interrupted가 되며 배포는
호스트에서 계속 중일 수 있다. systemd-logind가 `KillUserProcesses=yes`인 Linux 호스트(대부분 배포판의 기본값은 `no`)에서는
SSH 세션이 끝날 때 세션의 worker도 종료되므로, 실행 사용자를 `logind.conf`의 `KillExcludeUsers=`에 넣는다.

## 단일 원격 실행 검증 기록

2026-10-20 추가 검증:

- Rust 자동 테스트는 loopback SSH를 통해 원격 생성·배포·로그·endpoints·삭제 경로를 확인했다.
  로컬/원격의 메모리·포트 분리, 연결 실패 시 inventory/예약 보존, credential 교체와 호스트 등록 해제 거절을 검증했다.
- worker는 VM backend를 모의해 소유권·generation·create 재생·삭제 재생 방지·메모리 초과·실패 예약·잠금·외부 VM 보호를 검증했다.
- 실제 Linux aarch64 바이너리를 Docker로 빌드하고 임시 Rocky Linux OpenSSH 서버에 업로드했다.
  체크섬·프로토콜 handshake, 같은 digest 재사용, 실제 SDK의 빈 inventory 조회와 없는 Farm 삭제를 확인했다.
- 시험 Linux VM에 KVM이 없어 원격 KVM에서 실제 Farm 생성부터 OpenStack 배포 성공까지는 미검증이다.
  이 경로의 상태/로그 연결은 모의 backend 테스트이며 실제 KVM 검증으로 간주하지 않는다.

## 단일 VM 실행기로 업그레이드

새 Coordinator는 `supports_single_vm: true`인 실행기에서만 신규 Farm 생성을 허용한다.
이 필드가 없는 구형 실행기는 기존 Farm의 inventory·삭제 경로를 위해 유지하며 새 생성에는 사용하지 않는다.
실행기를 다시 빌드하고 `FARM_AGENT_PATH`를 지정해 Coordinator를 재시작한 뒤 Hosts의 실행기 업데이트를 실행한다.
기존 Farm의 고정 digest와 registry는 변경하지 않으며 다중 VM Farm의 재배포는 거절한다.

## 호스트 등록 해제와 VM 정리

호스트를 등록 해제할 때는 두 가지 방법이 있습니다.

- **기록만 제거** (기본 `DELETE /hosts/{id}`): coordinator의 호스트 기록과 암호화된 인증정보만
  지웁니다. 서버에 설치한 msb·이미지·VM·디스크는 그대로 남습니다. farm이 하나라도 살아 있으면
  `409`로 거절합니다.
- **VM까지 정리** (`DELETE /hosts/{id}?purge=true`): `204`가 아니라 `202`와 host job을 돌려줍니다.
  이 job은 (1) 그 호스트의 farm을 순서대로 삭제해 VM과 디스크를 회수하고, (2) agent에
  `Purge`를 보내 **그 agent가 만든 farm VM 전부**(다른 coordinator가 남긴 것 포함)를 정리한 뒤,
  (3) 호스트 기록과 인증정보를 지웁니다. 실행기가 오래 걸릴 수 있어 정리 작업에는 30분 예산을
  둡니다. 포탈의 **등록 해제** 대화상자에서 “Farm N개와 VM·디스크도 삭제”가 기본으로 켜져 있습니다.
- **스냅샷까지 정리** (`DELETE /hosts/{id}?purge=true&snapshots=true`): 위 정리에서 farm을 지운 뒤
  스냅샷도 모두 삭제합니다. 스냅샷은 로컬 호스트에만 보관하므로 로컬 호스트에만 쓸 수 있고, 다른
  호스트에 쓰면 `400`입니다. `purge=true` 없이 `snapshots=true`만 주어도 `400`입니다. 내보내지 않은
  스냅샷은 되돌릴 수 없어서 포탈의 체크박스는 기본으로 꺼져 있습니다. 스냅샷을 남긴 채 로컬 호스트를
  해제하면, 그 스냅샷을 지우려면 로컬 호스트를 다시 등록해야 합니다.

정리 대상은 farm이 만든 VM뿐입니다. agent는 자기 registry에 기록된 예약만 지우므로 서버에서
손으로 만든 msb 샌드박스는 유지됩니다. VM 삭제가 실패하면 호스트는 등록된 상태로 남고
host job에 실패 이유가 기록되므로, 원인을 고친 뒤 다시 시도할 수 있습니다. `Purge`를 모르는
예전 실행기가 설치돼 있으면 먼저 Hosts에서 실행기를 재설치하라는 오류가 납니다.

## 실행기 재설치와 farm

farm은 생성할 때 그 호스트의 실행기 digest를 고정합니다. 실행기를 재설치하면 **다음 배포부터**
farm이 호스트의 현재 실행기로 자동으로 옮겨갑니다(job 로그에 `agent: using the host's current agent …`).
호스트의 실행기가 이 coordinator와 다른 빌드면 옮기지 않고 `409`로 재설치를 요구합니다.

## 에이전트가 접속해 오는 호스트

SSH 대신 호스트의 agent가 coordinator로 WebSocket을 열어 두는 방식이다(`kind: "agent"`).
coordinator가 호스트에 접속할 수 없을 때(NAT 뒤의 노트북, 프로세스·SSH를 쓸 수 없는 서버리스 환경에 둔
coordinator) 사용한다.

1. `POST /api/v1/agent-hosts` `{"id":"edge"}` 또는 포탈의 **에이전트 호스트 등록**. 응답의 `token`은 다시 볼 수 없다.
   coordinator는 토큰의 SHA-256만 저장한다.
2. 호스트에서 `FARM_AGENT_TOKEN=<token> farm agent connect https://<coordinator>`를 실행한다
   (Linux 실행기는 `farm-agent connect <url>`). 끊기면 1초부터 30초까지 늘려 가며 다시 접속한다.
3. 연결되면 점검·환경 준비를 실행한다. 점검, msb 설치와 시험 VM 부팅은 그 호스트의 agent가 직접 한다.
   msb가 없거나 0.7.13보다 낮으면 SSH 호스트와 같은 설치 스크립트로 공식 릴리스를 설치·업그레이드한다
   (Linux, Apple Silicon macOS. 그 외 플랫폼은 직접 설치). golden image는 첫 farm이 빌드한다.

요청·진행·결과는 SSH 실행기와 같은 프로토콜을 세션 번호로 나눠 한 소켓에 싣는다. 접속에는 API token이 아니라
호스트 토큰을 `Authorization: Bearer`로 쓴다. 연결이 끊기면 그 소켓의 진행 중인 작업은 실패로 끝나고,
agent 쪽에서도 함께 중단된다(SSH가 끊길 때와 같다). 배포만은 SSH 호스트처럼 분리된 worker로 계속되고,
agent가 다시 접속하면 coordinator가 다시 붙는다. agent를 systemd 서비스로 돌린다면 서비스를 멈출 때
cgroup의 worker도 함께 종료되므로(`KillMode=control-group`) 배포 중에는 서비스를 재시작하지 않는다.
연결되지 않은 호스트에는 Farm을 만들 수 없다.
public 포트는 원격 호스트처럼 `0.0.0.0`에 바인딩한다.

