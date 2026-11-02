# 스냅샷: 얼리기와 불러오기

OpenStack 설치에는 수십 분이 걸립니다. 설치가 끝난 farm의 VM을 **스냅샷으로 얼려두면**
같은 환경을 다시 설치하지 않고 새 farm으로 불러올 수 있습니다. 스냅샷은 `.msb` 파일로
내보내 다른 호스트에서 가져올 수도 있습니다. 스냅샷은 Coordinator가 실행 중인 머신의 로컬 호스트
(`kind: "local"`)에 보관하므로, 그 호스트의 farm만 얼리고 그 호스트로만 복원합니다.

```bash
farm snapshot create dev dev-clean --note "Nova·Neutron clean 설치"
farm create dev2 --host-id local --snapshot dev-clean      # 배포된 스냅샷이면 바로 ready
farm snapshot export dev-clean -o ~/farm-snapshots/
farm snapshot import ~/farm-snapshots/dev-clean.msb --id dev-clean
farm snapshot export dev-clean --bucket team-r2                 # 등록한 S3 버킷으로
farm snapshot import --bucket team-r2 --key farm/dev-clean.msb  # 다른 호스트에서
farm snapshot list
farm snapshot delete dev-clean
```

포탈에서는 Farm 상세의 **얼리기**, `Snapshots` 메뉴의 **Farm 만들기·내보내기·가져오기·삭제**를 사용합니다.

## 재시작과 서비스 확인

farm의 VM을 직접 재시작하거나 호스트가 재부팅하면 farmd는 VM이 잠시 멈춘 것을 보고 farm을
`broken`, 이어서 `created`로 표시합니다. 이때는 배포를 다시 실행하지 말고 **서비스 확인**을
실행하세요(`farm settle <id>`, `POST /farms/{id}/settle`). Keystone과 선택 서비스가 응답하면
farm이 다시 `ready`가 됩니다. 복원한 farm도 이 방법으로 상태를 갱신할 수 있습니다.

```bash
msb stop dev && msb start dev   # 종료는 시간이 걸리므로 stop/start를 권장
farm settle dev
```

`msb restart`는 종료를 10초만 기다리고 `farm-identity`의 dummy 인터페이스가 사라진 상태로
부팅할 수 있습니다. 그때는 `msb stop`이 끝난 뒤 `msb start`로 다시 시작하세요.

## 고정 게스트 정체성

복원한 VM은 새 샌드박스 이름으로 부팅하고, msb가 매번 시작할 때 고르는 eth0 주소(`172.16.x.y/30`)도
달라집니다. 일반 재시작에서도 주소가 바뀔 수 있습니다. 배포된 OpenStack은 MariaDB·RabbitMQ·서비스
bind 주소와 RabbitMQ 노드 이름(`rabbit@<hostname>`), nova-compute·Neutron agent의 host 이름에 묶여
있으므로, 이 값이 바뀌면 동작하지 않습니다.

그래서 새 farm은 모두 같은 게스트 정체성을 가집니다.

- hostname `farm-aio`
- dummy 인터페이스 `farm0`의 서비스 주소 `10.254.0.2/24`. Kolla의 `network_interface`,
  `api_interface`, `kolla_internal_vip_address`가 이 주소를 사용합니다.
- eth0으로 들어오는 published 포트(5000·9292·8780·80·9696·8774)는 nftables DNAT로 `farm0`에 전달합니다.

`farm-identity.service`가 부팅할 때마다 `/etc/farm/identity.conf`를 읽어 위 설정을 적용하고,
`farm-hostname.service`가 hostname을 서비스 주소로 매핑한 뒤 Docker가 시작합니다.
이 설정은 farm마다 독립된 VM 안에서만 쓰이므로 farm끼리 충돌하지 않습니다.

이 기능 이전에 만든 farm은 eth0 주소로 배포되어 있어 얼릴 수 없습니다(`409`).
새 farm을 만들고 배포한 뒤 그 farm을 얼리세요.

## 얼리기

1. VM을 정상 종료합니다. MariaDB와 RabbitMQ가 깨끗하게 닫힙니다.
2. root disk와 Docker·containerd 디스크를 disk 스냅샷으로 저장합니다(msb 그룹 `farm-snap-<id>`).
3. VM을 다시 시작하고, 배포된 farm이면 Keystone·Glance와 선택 서비스(Neutron agent,
   nova-compute)가 다시 응답할 때까지 기다립니다.

원본 farm은 이 동안 몇 분간 응답하지 않습니다. 상태는 그대로 유지되고, 실패는 job과 스냅샷에 기록됩니다.
캡처는 성공했지만 원본이 다시 시작되지 않으면 스냅샷은 `ready`, job은 `failed`가 됩니다.
메모리까지 저장하는 full 스냅샷은 macOS(HVF)에서 msb가 지원하지 않아 사용하지 않습니다.

## 불러오기

`POST /farms`에 `snapshot_id`를 지정하면 `restore` job이 실행됩니다. 서비스 목록과 VM 크기는
스냅샷의 계획을 사용하므로 `components`는 생략합니다. 메모리·포트 예약은 일반 생성과 같습니다.

1. 스냅샷에서 `<id>` VM을 콜드부팅하고 새 farm의 포트를 publish합니다.
2. 복원한 VM에는 farm 라벨이 없으므로 `farm`·`farm.role` 라벨을 저장합니다(`next_start`, 재시작 없음).
   all-in-one VM은 이름(`<farm>`, 이전 버전은 `<farm>-all-in-one`)으로 farm을 판단하므로 원본 farm에 섞이지 않습니다.
3. 배포된 스냅샷이면 Keystone 카탈로그의 public endpoint를 원본의 `host:port`에서 새 farm의
   `host:port`로 바꾸고, `/etc/kolla/globals.yml`도 같이 고친 뒤 서비스 응답을 기다립니다.

배포 전에 얼린 스냅샷은 `created` 상태로 복원되며 일반 farm처럼 배포합니다.
복원한 farm도 다시 배포할 수 있습니다.

## 아키텍처와 플랫폼

스냅샷은 게스트의 부트로더와 커널을 그대로 담으므로 CPU 아키텍처를 넘지 못합니다. 얼릴 때
아키텍처(`arch`)와 그 호스트의 OS(`platform`)를 기록하고, **가져오기와 복원 양쪽에서** 다른
아키텍처를 거절합니다. Apple Silicon에서 얼린 `.msb`를 x86_64 Linux 호스트로 옮기면
`archive was frozen on aarch64; this host is x86_64`로, 상태 파일만 옮겨온 경우에는 복원 단계에서
`cannot boot on this x86_64 host`로 실패합니다. 상태 파일과 msb 스냅샷 디렉터리를 서로 다른
아키텍처로 복사하지 마세요.

`platform`은 복원 가능 여부를 막지는 않고, 포탈 스냅샷 카드에 `aarch64 · macOS`처럼 표시됩니다.
같은 아키텍처라면 macOS에서 얼린 게스트를 Linux의 microsandbox로 복원할 수 있지만, 서버 간
이동이 잦다면 대상 플랫폼에서 얼리는 편이 안전합니다. 현재 호스트의 아키텍처와 다른 스냅샷은
카드에서 경고색으로 표시되고 복원 버튼이 비활성화됩니다.

OpenStack 릴리스는 label(`farm.snapshot.openstack-release`)로만 기록하고 복원을 막지는 않습니다.
스냅샷은 자기 Kolla 설치를 그대로 담기 때문입니다. 다만 서로 다른 릴리스에서 같은 farm을
다시 배포하면 서비스 이미지가 섞일 수 있으니, 복원 후에는 재배포 대신 서비스 확인을 쓰세요.

## 내보내기와 가져오기

내보내기는 golden image까지 포함한 self-contained 아카이브(tar+zstd)를 씁니다. 기본 경로는
`<state dir>/exports/<id>.msb`입니다. 경로에 폴더를 주면 그 안에 `<id>.msb`로 저장하고,
기존 파일은 덮어쓰지 않습니다. 완료될 때까지 `<path>.partial`에 쓴 뒤 이름을 바꿉니다.

스냅샷 메타데이터(서비스 목록, 배포 여부, 원본 public 포트, OpenStack 릴리스, CPU 아키텍처, 메모)는
msb 스냅샷 label로 저장되며 아카이브 안에 함께 들어갑니다. 가져오기는 이 label로 스냅샷을 다시
등록하고, label이 없거나 아키텍처가 다른 아카이브는 거절하고 정리합니다. ID를 생략하면 파일 이름을
사용합니다. 경로는 모두 farmd 호스트의 절대 경로입니다.

S3 버킷으로 옮기는 방법은 [S3 버킷](#s3-버킷)을 참고하세요.

내보내기와 가져오기는 job이 아니라 스냅샷 상태(`exporting`, `importing`)로 진행됩니다.
데몬이 중간에 종료되었을 때 다시 시도할 수 있는 상태로 바꿉니다.
스냅샷 삭제는 내보낸 파일을 지우지 않습니다. `msb snapshot rm`으로 디스크를 먼저 지웠다면 호스트
에이전트가 이미 없는 스냅샷으로 보고 삭제를 성공으로 처리하므로, 기록도 함께 지워집니다.
스냅샷을 보관하던 호스트가 없어졌거나 접속되지 않으면 `DELETE /snapshots/{id}?forget=true`
(`farm snapshot delete <id> --forget`, 포탈의 **기록만 삭제**)로 coordinator 기록만 지울 수 있습니다.
이때 호스트에 남은 디스크는 지우지 않습니다.

## S3 버킷

아카이브는 S3 API를 쓰는 저장소로 바로 올리고 가져올 수 있습니다. AWS S3, Cloudflare R2, MinIO,
RustFS에서 확인했습니다. 버킷은 `Snapshots` 화면 아래의 **S3 버킷**(`POST /buckets`)에서 미리
등록합니다.

| 항목 | 설명 |
|---|---|
| Endpoint | `https://<account-id>.r2.cloudflarestorage.com`, `http://minio:9000`처럼 scheme·호스트·포트만. AWS S3는 비웁니다. |
| Region | 서명에 쓰입니다. 기본값 `us-east-1`, R2는 `auto`, AWS는 버킷의 리전 |
| Prefix | 내보내기 기본 키는 `<prefix><id>.msb`, 가져오기 목록은 이 prefix 아래의 `.msb` |
| Path-style | `endpoint/버킷/키`. endpoint를 지정하면 기본으로 켜집니다(MinIO·RustFS). |
| 사설 CA | PEM 인증서. 시스템 인증서와 함께 신뢰합니다. |

Secret key는 호스트 SSH 자격 증명과 같은 `credentials.key`(XChaCha20-Poly1305)로 암호화해
`state.json`에 저장하고, API로 다시 돌려주지 않습니다. 암호문은 endpoint·버킷·access key에 묶여
있어 상태 파일에서 endpoint만 바꾸면 복호화되지 않습니다. 설정을 고칠 때 secret을 비워 두면 저장된
키를 유지하지만, endpoint·버킷·access key를 바꿀 때는 다시 입력해야 합니다. **연결 확인**
(`POST /buckets/test-connection`, `POST /buckets/{id}/check`)은 prefix 아래에서 목록 조회, 작은
객체 쓰기와 삭제를 해 봅니다.

내보내기는 `<state dir>/transfers`에 아카이브를 만든 뒤 multipart로 올리고, 다 올리면 로컬
파일을 지웁니다. 그동안 스냅샷 크기만큼 디스크가 필요합니다. 키가 `.msb`로 끝나지 않으면 붙이고,
같은 키의 객체가 있으면 덮어쓰지 않습니다. 가져오기는 같은 디렉터리로 내려받아(끊기면 Range로 이어받음) 등록한 뒤 지웁니다. 진행
상황은 스냅샷의 `transfer`(`done_bytes`/`total_bytes`)로 보입니다.

S3 호환 서버마다 다른 동작을 피하려고 다음처럼 요청합니다.

- SigV4 서명, 본문은 SHA-256으로 서명합니다(`aws-chunked`·`x-amz-checksum-*` 헤더 없음).
- part는 마지막을 빼고 모두 같은 크기입니다(최소 64 MiB, 10,000개 안에 들어가게). R2가 요구합니다.
- 실패한 업로드는 abort합니다. abort하지 못했거나 데몬이 업로드 중에 멈췄다면 업로드 ID를
  기록해 두었다가 다음 시작 때 abort하므로, 미완료 part가 요금으로 남지 않습니다.
- redirect를 따라가지 않습니다. 리전이 틀리면 서버 오류 메시지가 그대로 스냅샷 오류에 남습니다.

버킷 등록을 해제해도 버킷에 올린 아카이브는 남습니다. 진행 중인 전송이 쓰는 버킷은 수정하거나
해제할 수 없습니다(`409`). 실제 서비스로 클라이언트를 확인하려면
`FARM_S3_BUCKET`, `FARM_S3_ACCESS_KEY`, `FARM_S3_SECRET_KEY`(필요하면 `FARM_S3_ENDPOINT`,
`FARM_S3_REGION`, `FARM_S3_PREFIX`, `FARM_S3_PATH_STYLE`, `FARM_S3_CA_FILE`)를 지정하고
`cargo test s3_live -- --ignored`를 실행합니다.

## API

| Method | Path (`/api/v1` 기준) | 반환 |
|---|---|---|
| POST | `/farms/{id}/snapshots` | `{"id","note"}` → snapshot job (202) |
| GET | `/snapshots`, `/snapshots/{id}` | 스냅샷 목록 / 상세 |
| DELETE | `/snapshots/{id}` | 스냅샷 삭제 (`?forget=true`: 기록만 삭제) |
| POST | `/snapshots/{id}/export` | `{"path"}` 또는 `{"bucket","key"}` 선택 → 내보내기 시작 (202) |
| POST | `/snapshots/import` | `{"path","id"}` 또는 `{"bucket","key","id"}` → 가져오기 시작 (202) |
| GET / POST | `/buckets` | S3 버킷 목록 / 등록 (secret은 응답에 없음) |
| GET / PUT / DELETE | `/buckets/{id}` | 버킷 상세 / 수정 / 등록 해제 |
| POST | `/buckets/test-connection`, `/buckets/{id}/check` | 저장 전 / 등록된 버킷 연결 확인 |
| GET | `/buckets/{id}/objects` | prefix 아래의 `.msb` 객체 (최신순) |
| POST | `/farms` | `{"id","snapshot_id",...}` → restore job (202) |
| POST | `/farms/{id}/settle` | 서비스 응답 확인 job (배포 재실행 없음) |

스냅샷 상태는 `creating`, `ready`, `exporting`, `importing`, `deleting`, `failed`입니다.
