# farm

microsandbox VM으로 OpenStack farm을 만드는 도구.

```bash
task docker-build              # ubuntu:24-golden 이미지 빌드 후 msb 캐시에 로드
cargo build --release

./target/release/farm create demo   # deployer / infra / keystone / glance VM 생성
./target/release/farm status demo
./target/release/farm list
./target/release/farm delete demo
```

## 구조

| 파일 | 역할 |
|---|---|
| `src/farm/template.rs` | 베이스 이미지, role 목록, role별 CPU·메모리·디스크 |
| `src/farm/node.rs` | VM 하나 생성·조회·삭제 (microsandbox SDK) |
| `src/farm/mod.rs` | farm 단위 생성·조회·삭제·목록 |
| `src/main.rs` | CLI 진입점 |

VM 이름은 `<farm>-<role>`이고, `farm`·`farm.role` 라벨로 farm에 묶입니다.

실행하려면 `msb`와 `libkrunfw`가 `~/.microsandbox`에 설치되어 있어야 합니다 (`MSB_PATH`, `MSB_LIBKRUNFW_PATH`로 지정 가능).
