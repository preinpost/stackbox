# farm 개발 지침

farm은 microsandbox VM 위에 Kolla로 OpenStack을 설치하는 Rust CLI·REST API 데몬(`farm`)과
웹포탈(`web/`)입니다. farm 하나는 control VM 1대와 compute VM 0~3대(기본 1대)로 이루어집니다.

## 지원 범위

- **OpenStack 2024.1(Caracal) 이상만 지원합니다.** `src/kolla/releases.rs`의
  `MIN_SUPPORTED_RELEASE`가 기준입니다. 릴리스 카탈로그와 `POST /releases/plan`,
  `POST /releases/farms`, `POST /plans`, `POST /farms`는 그보다 오래된 릴리스를 거절합니다.
- 2022.2(zed) 이하 EOL 지원 코드는 남아 있습니다. 게스트 Python, 커널 패치, 레거시 이미지
  경로가 여기에 해당합니다. 이 시리즈는 `FARM_ALLOW_UNSUPPORTED_RELEASES=1`을 설정한
  coordinator에서만 카탈로그에 나오고 생성할 수 있습니다. 2023.1·2023.2는 이미지가 없어
  지원하지 않습니다.
- 기존 farm은 기록된 릴리스와 상관없이 조회·배포·compute 추가·삭제가 계속 동작해야 합니다.
  `state.json`의 farm·스냅샷 스키마를 깨지 마세요.

## 릴리스별 Kolla 이미지

farm이 이미지를 빌드해 쓰는 릴리스(2024.1 이상과 2022.2)는 `kolla_base_distro: rocky`이고,
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
  있습니다. 이 저장소는 그 이미지를 소비할 뿐이며, 로컬의 `microsandbox/kolla-images` 체크아웃은
  별도 작업 공간입니다.
- registry와 namespace는 `src/kolla/releases.rs`의 `EOL_REGISTRY`·`EOL_NAMESPACE` 한 곳에서
  정합니다. 사설 미러는 `FARM_EOL_REGISTRY`·`FARM_EOL_NAMESPACE`로 바꾸며, 값이 registry 경로에
  쓸 수 있는 문자(`[A-Za-z0-9._:/-]`)가 아니면 경고를 남기고 기본값을 씁니다.
- 게스트는 Rocky 10과 Python 3.12입니다. 2024.1의 requirements에는 Ansible이 없어 설치 단계가
  ansible-core 2.16을 넣고, 2024.2는 requirements의 ansible-core 2.16~2.18을 씁니다. 2025.1 이전
  릴리스에는 호스트 OS precheck·SELinux·Neutron 모듈 호환 값이 globals에 들어갑니다.
- `arches`와 `ReleaseInfo.arm64`는 실제로 게시된 이미지를 따라야 합니다. farm은 arm64용으로
  배포판을 바꾸지 않으므로 2026.2 이전 릴리스는 amd64 전용입니다.

## 개발

```bash
task dev                        # coordinator + 포탈 개발 서버
cargo test
cargo fmt --check
cargo clippy --all-targets
npm --prefix web run build      # tsc -b + vite build
cd web && npx vitest run
```

- API 타입을 바꾸면 `farm openapi`로 `docs/openapi.json`을 다시 만들고
  `npm --prefix web run api:types`로 `web/src/api/schema.d.ts`를 갱신합니다.
- `task golden`과 `task agent-build`는 실제 VM·Docker가 필요합니다.
