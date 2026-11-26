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
| 2026.2 이상 | `stable/<rel>` 또는 `unmaintained/<rel>` | amd64·aarch64: `quay.io/openstack.kolla/<svc>:<rel>-rocky-<major>[-aarch64]` | amd64, arm64 |
| 2025.2 | `stable/2025.2` 또는 `unmaintained/2025.2` | amd64: `quay.io/openstack.kolla/<svc>:2025.2-rocky-10`; aarch64: `ghcr.io/preinpost/kolla/<svc>:2025.2-rocky-10-aarch64` (자체 빌드; pbr 7.0.3 치환, pip 24.0 핀 생략 — preinpost/kolla `.github/kolla/template-override.j2`) | amd64, arm64 |
| 2025.1, 2026.1 | `stable/<rel>` 또는 `unmaintained/<rel>` | amd64: `quay.io/openstack.kolla/<svc>:<rel>-rocky-<major>`; aarch64: `ghcr.io/preinpost/kolla/<svc>:<rel>-rocky-<major>-aarch64` (자체 빌드) | amd64, arm64 |
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
- 2025.1·2025.2·2026.1의 aarch64 이미지는 quay가 Debian/Ubuntu 기반으로만 제공하거나(2025.2는 Rocky 10 aarch64가 없음) 빌드 전 kolla 소스 수정이 필요하므로, stackbox가 Rocky 기반
  aarch64 이미지를 직접 빌드해 `ghcr.io/preinpost/kolla`에 `<rel>-rocky-<major>-aarch64` 태그로 올립니다
  (`github.com/preinpost/kolla`의 `eol-images` workflow, `arch=arm64`). 이 목록은
  `src/kolla/releases.rs`의 `OWN_ARM64` 상수 하나이며, 빌드가 실패한 릴리스는 거기서 뺍니다.
  그 릴리스의 `ReleaseInfo.arm64_images`가 ghcr registry·namespace를 가리키고, amd64 이미지는 quay에 그대로 둡니다.
- 노드마다 이미지 출처가 다를 수 있습니다. 각 VM의 `uname -m`으로 suffix·registry·namespace를 고르고
  (`src/kolla/config.rs`의 `image_source`), 혼합 아키텍처 multinode는 다음 원리로 동작합니다. globals.yml은
  kolla-ansible의 extra vars라 호스트별 값을 직접 가질 수 없으므로 `openstack_tag_suffix`·`docker_registry`·
  `docker_namespace`를 `{{ stackbox_tag_suffix | default(control값) }}` 같은 Jinja로 쓰고, 인벤토리의 compute 줄이
  `stackbox_tag_suffix`·`stackbox_docker_registry`·`stackbox_docker_namespace` 호스트 변수로 노드별 값을 싣습니다.
  각 노드는 자기 변수를 읽고 값이 없으면 control 값을 씁니다. 기존 globals.yml은 compute 추가 시 `per_host_images`가 같은 형태로 바꿉니다.
- `arches`와 `ReleaseInfo.arm64`는 실제로 게시된 이미지를 따릅니다. 2026.2는 quay의 Rocky aarch64,
  2025.1·2025.2·2026.1은 자체 빌드, 2024.x·zed는 amd64 전용입니다. stackbox는 arm64용으로 배포판을 바꾸지 않습니다.

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

## 릴리스 절차

릴리스는 `.github/workflows/release.yml`이 main push로 만듭니다. 태그와 버전 커밋은 사람이 만들지 않습니다.
게스트 커널(`libkrunfw.yml`)은 별도 주기로 빌드하므로 릴리스와 무관합니다.

1. **dev에서 버전 범프.** `sh scripts/set-version.sh X.Y.Z`로 `Cargo.toml`, `Cargo.lock`,
   `web/package.json`, `web/package-lock.json` 4개를 함께 바꾸고 한 커밋으로 남깁니다(예: `2776a00 Set the version to 0.1.19`).
   커밋 메시지는 `Release v`로 시작하면 안 됩니다. 그 커밋이 main에 올라가면 gate가 건너뛰어져 아무것도 릴리스되지 않습니다.
2. **로컬 사전 게이트.** CI와 같은 항목을 통과시킵니다.
   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   (cd web && npx vitest run)
   node --test web/server/*.test.mjs
   npm --prefix web run build
   sh scripts/build-agent.sh windows-x86_64   # src/remote 변경 시 특히 (std::os::unix 가드 누락으로 과거 실패)
   ```
3. **dev push.** `git push origin dev`로 `ci.yml`(같은 게이트)을 기다려 통과를 확인합니다.
4. **main fast-forward push.** dev 커밋을 main에 fast-forward로만 올립니다(`git push origin dev:main`). force push는 금지입니다.
   이 push가 `release.yml`을 트리거합니다.

`release.yml`의 순서는 다음과 같습니다.

- **gate**: head 커밋 메시지가 `Release v`로 시작하면 건너뜁니다(봇의 릴리스 커밋이 재귀로 트리거되지 않게 하는 조건).
  그 외에는 CI와 같은 게이트를 실행합니다.
- **prepare**: `Cargo.toml` 버전이 `v<버전>` 태그로 아직 없으면 그 버전을 그대로 씁니다(자동 증가 없음).
  이미 태그되어 있으면 마지막 태그 이후 커밋의 `#major`/`[major]`, `#minor`/`[minor]` 표시를 보고 올리며, 표시가 없으면 patch를 올립니다.
  결과 버전 태그가 이미 있으면 실패합니다. 그래서 범프 없이 main에 push하면 patch가 자동으로 올라갑니다.
- **build**: linux-x86_64, linux-aarch64, macos-aarch64, windows-x86_64 4개 agent를 빌드하고 각 파일에 build id 스탬프가 하나씩 있는지 확인합니다.
- **publish**: 4개 agent의 build id가 모두 같은지 확인하고 `manifest.json`을 만듭니다. 봇이 `Release v<버전>` 커밋과 `v<버전>` 태그를 만들고
  main과 태그를 `--atomic`으로 함께 push한 뒤 GitHub Release를 게시합니다(자산 5개: agent 4개 + manifest.json).
- **docker**: `ghcr.io/preinpost/stackbox`에 `v<버전>`, `latest`, `sha-<7자리>` 태그로 이미지를 푸시합니다.
- **merge back**: `dev`에 `Merge v<버전> into dev` 머지 커밋을 push합니다. 다음 작업 전에 로컬 dev를 `git pull`로 맞춥니다.

수동으로 실행하려면 Actions에서 `workflow_dispatch`(bump: patch/minor/major)를 쓸 수 있습니다. 실행 대상은 main의 현재 HEAD입니다.

태그를 수동으로 만들지 마세요. 워크플로는 현재 버전의 태그를 보고 다음 버전을 정하므로, 손으로 만든 태그가 있으면 버전이 어긋나거나 원자적 push가 실패합니다.

### 사후 검증

```bash
gh run list --workflow release.yml --limit 1   # 최신 run id 확인
gh run view <run-id>                           # gate·prepare·build·publish·docker가 모두 success인지 확인
gh run list --workflow ci.yml --limit 1        # dev push의 ci 확인
gh release view v<버전>                         # 자산 5개: stackbox-agent-{플랫폼}[.exe] 4개 + manifest.json
```

- 자산 검증: 저장소 밖의 빈 디렉터리에 `curl -fsSLO https://github.com/preinpost/stackbox/releases/download/v<버전>/<파일>`로 받고,
  `sha256sum`이 `manifest.json`의 `agents.<플랫폼>.sha256`과 같은지, `grep -aoE 'stackbox-agent-build:[0-9a-f]{16}'`가
  manifest의 `build_id`를 포함하는지 확인합니다. 받은 파일은 실행하지 않습니다.
- GHCR 검증: 익명 토큰을 받은 뒤 `https://ghcr.io/v2/preinpost/stackbox/manifests/v<버전>`를 요청해 200이 오는지 봅니다.
  토큰 없이 처음 요청하면 401이 나오는 것이 정상입니다. 토큰 파일은 읽지 말고 `gh` 인증만 씁니다.
