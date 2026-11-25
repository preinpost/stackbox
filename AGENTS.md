# stackbox 개발 지침

stackbox는 microsandbox VM 위에 Kolla로 OpenStack을 설치하는 Rust CLI·REST API 데몬(`stackbox`)과
웹포탈(`web/`)입니다. stackbox 하나는 control VM 1대와 compute VM 0~3대(기본 1대)로 이루어집니다.

## 지원 범위

- **OpenStack 2024.1(Caracal) 이상만 지원합니다.** `src/kolla/releases.rs`의
  `MIN_SUPPORTED_RELEASE`가 기준입니다. 릴리스 카탈로그와 `POST /releases/plan`,
  `POST /releases/stackboxes`, `POST /plans`, `POST /stackboxes`는 그보다 오래된 릴리스를 거절합니다.
- 2022.2(zed) 이하 EOL 지원 코드는 남아 있습니다. 게스트 Python, 커널 패치, 레거시 이미지
  경로가 여기에 해당합니다. 이 시리즈는 `STACKBOX_ALLOW_UNSUPPORTED_RELEASES=1`을 설정한
  coordinator에서만 카탈로그에 나오고 생성할 수 있습니다. 2023.1·2023.2는 이미지가 없어
  지원하지 않습니다.
- 기존 stackbox는 기록된 릴리스와 상관없이 조회·배포·compute 추가·삭제가 계속 동작해야 합니다.
  `state.json`의 stackbox·스냅샷 스키마를 깨지 마세요.

## 릴리스별 Kolla 이미지

stackbox가 이미지를 빌드해 쓰는 릴리스(2024.1 이상과 2022.2)는 `kolla_base_distro: rocky`이고,
이미지 태그는 kolla-ansible의 `<release>-rocky-<major>` 규칙을 따릅니다. 이미지를 빌드하지 않은
EOL 시리즈를 opt-in으로 켜면 그 태그의 기본값(`ubuntu`)과 그 레지스트리가 가리키는 이미지를
그대로 씁니다.

| 릴리스 | kolla-ansible ref | 이미지 | arch |
|---|---|---|---|
| 2025.1 이상 | `stable/<rel>` 또는 `unmaintained/<rel>` | `quay.io/openstack.kolla/<svc>:<rel>-rocky-<major>[-aarch64]` | amd64, 2026.2만 arm64 |
| 2024.1 | `unmaintained/2024.1` | `ghcr.io/preinpost/kolla/<svc>:2024.1-rocky-9` | amd64 |
| 2024.2 | `2024.2-eol` (브랜치 삭제됨) | `ghcr.io/preinpost/kolla/<svc>:2024.2-rocky-9` | amd64 |
| 2022.2(zed) | `15.6.0` | `ghcr.io/preinpost/kolla/<svc>:zed-rocky-9` (opt-in) | amd64 |
| 2022.1 이하 (opt-in) | 마지막 태그(`14.11.0` 등) | 태그 기본값 그대로(`ubuntu` 기반), 게시된 이미지 없음 | amd64 |

- 2024.1·2024.2와 zed 이미지는 `github.com/preinpost/kolla`의
  `.github/workflows/eol-images.yml`이 빌드하고, 이미지 목록은 `.github/kolla/groups.json`에
  있습니다. 이 저장소는 그 이미지를 소비할 뿐이며, 이미지 빌드는 `github.com/preinpost/kolla`에서
  관리합니다. 이 저장소에서 이미지를 직접 빌드하지 않습니다.
- registry와 namespace는 `src/kolla/releases.rs`의 `EOL_REGISTRY`·`EOL_NAMESPACE` 한 곳에서
  정합니다. 사설 미러는 `STACKBOX_EOL_REGISTRY`·`STACKBOX_EOL_NAMESPACE`로 바꾸며, 값이 registry 경로에
  쓸 수 있는 문자(`[A-Za-z0-9._:/-]`)가 아니면 경고를 남기고 기본값을 씁니다.
- 게스트는 Rocky 10과 Python 3.12입니다. 2024.1의 requirements에는 Ansible이 없어 설치 단계가
  ansible-core 2.16을 넣고, 2024.2는 requirements의 ansible-core 2.16~2.18을 씁니다. 게스트 커널은
  모듈을 로드할 수 없으므로 모든 릴리스의 globals에 `neutron_modules_default: []`가 들어가고,
  2025.1 이전 릴리스에는 호스트 OS precheck·SELinux 호환 값도 들어갑니다. 2025.1의 Kolla 이미지는
  Rocky 9(Python 3.9)이고, 2025.2부터 Rocky 10(Python 3.12)입니다.
- `arches`와 `ReleaseInfo.arm64`는 실제로 게시된 이미지를 따라야 합니다. stackbox는 arm64용으로
  배포판을 바꾸지 않으므로 2026.2 이전 릴리스는 amd64 전용입니다.

## 개발

```bash
task dev                        # coordinator + 포탈 개발 서버; 커널 빌드(Docker) 선행, 없으면 건너뜀
cargo test
cargo fmt --check
cargo clippy --all-targets
npm --prefix web run build      # tsc -b + vite build
cd web && npx vitest run
```

- API 타입을 바꾸면 `stackbox openapi`로 `docs/openapi.json`을 다시 만들고
  `npm --prefix web run api:types`로 `web/src/api/schema.d.ts`를 갱신합니다.
- `task golden`과 `task agent-build`는 실제 VM·Docker가 필요합니다.

## 테스트와 셸 스크립트

- coordinator와 agent 경계를 넘는 상태는 그 경계를 실제로 통과하는 테스트로 검증합니다. 로컬
  agent는 요청마다 자식 프로세스로 뜨므로(`src/remote/link.rs`의 `ProcessLink`) agent 프로세스의
  정적·메모리 상태는 다음 요청에 남지 않습니다. 검증 결과(`HostInspection.mtty_available` 등)는
  응답(`Reply`)에 실어 coordinator가 저장해야 하며, 단위 테스트만으로는 이런 누락을 잡지 못합니다
  (`src/manager/tests.rs`의 `agent_host_preparation_keeps_the_verification_replys_mtty_result`).
- 생성 셸 스크립트와 `src/shell.rs` 래퍼는 `set -o pipefail` 아래에서 돕니다. 파이프 앞쪽이 아직
  쓰는 중에 읽기를 일찍 끝내는 명령(`grep -q`, `head`, `grep -m`)은 쓰지 않습니다. 앞 명령(ssh,
  docker 등)이 SIGPIPE로 끝나 파이프라인 전체가 실패하므로 `grep ... >/dev/null`을 쓰거나 출력을
  변수에 담아 검사합니다. `src/kolla/mod.rs`의 mtty 검사와 그 테스트
  `the_mtty_check_reads_the_libvirt_xml_to_the_end`가 이 형태입니다.
- 실환경 E2E는 메모리 예산을 먼저 계산합니다. 기본 예산은 호스트 RAM의 80%이고
  (`src/manager/mod.rs`의 `memory_budget_mib`, 변경은 `--memory-budget-mib`, `src/main.rs`), 스냅샷
  복원 stackbox도 원본과 같은 메모리를 예약합니다(`src/manager/mod.rs`의 `JobKind::Restore` 생성 경로). 원본과
  복원을 동시에 띄우면 기존 예약 + 원본 + 복원 합계가 예산 안인지 시작 전에 계산합니다. 넘으면
  `POST /stackboxes`가 409 `memory budget of host '<host>' exceeded: ...`로 거절합니다(`src/manager/mod.rs`의
  `memory budget of host` 오류, `src/api.rs`의 `StatusCode::CONFLICT`).
- daemon(`stackbox serve`)이 실행 중이면 같은 state dir의 `stackbox` CLI는 `stackbox state is locked by another
  process`로 실패합니다(`src/manager/store.rs`). 이때는 daemon의 REST API(`docs/openapi.json`)를 쓰거나 별도
  state dir(README "코드와 검증" 절의 `mktemp`)을 씁니다. `scripts/verify-openstack.py`는 내부에서
  `stackbox status`·`stackbox endpoints --json`을 호출하므로(`scripts/verify-openstack.py`) daemon 실행 중에는
  `--state-dir`만으로 안 되고, `--stackbox-binary`에 API를 중계하는 래퍼(두 하위 명령 지원)를 넘깁니다.
