# 스냅샷: 얼리기와 불러오기

OpenStack 설치에는 수십 분이 걸립니다. 설치가 끝난 stackbox의 VM을 **스냅샷으로 얼려두면**
같은 환경을 다시 설치하지 않고 새 stackbox로 불러올 수 있습니다. 스냅샷은 `.msb` 파일로
내보내 다른 호스트에서 가져올 수도 있습니다. 스냅샷 원본은 stackbox를 얼린 호스트에 남습니다. 원격
호스트에서 얼린 스냅샷은 기록의 `host_id`가 그 호스트를 가리키고, 로컬 호스트(`kind: "local"`)에서
얼리거나 가져온 스냅샷, 그리고 이 필드 이전의 스냅샷은 `host_id`가 없습니다. `arch`와 `platform`도
coordinator가 아니라 얼린 호스트의 것입니다.

```bash
stackbox snapshot create dev dev-clean --note "Nova·Neutron clean 설치"
stackbox create dev2 --host-id local --snapshot dev-clean      # 배포된 스냅샷이면 바로 ready
stackbox snapshot export dev-clean -o ~/stackbox-snapshots/
stackbox snapshot import ~/stackbox-snapshots/dev-clean.msb --id dev-clean
stackbox snapshot export dev-clean --bucket team-r2                 # 등록한 S3 버킷으로
stackbox snapshot import --bucket team-r2 --key stackbox/dev-clean.msb  # 다른 호스트에서
stackbox snapshot bucket team-r2                                     # 버킷의 아카이브와 메타데이터
stackbox snapshot list
stackbox snapshot delete dev-clean
```

포탈에서는 Stackbox 상세의 **얼리기**, `Snapshots` 메뉴의 **Stackbox 만들기·내보내기·가져오기·삭제**를 사용합니다.

## 재시작과 서비스 확인

stackbox의 VM을 직접 재시작하거나 호스트가 재부팅하면 stackboxd는 VM이 잠시 멈춘 것을 보고 stackbox를
`broken`, 이어서 `created`로 표시합니다. 이때는 배포를 다시 실행하지 말고 **서비스 확인**을
실행하세요(`stackbox settle <id>`, `POST /stackboxes/{id}/settle`). Keystone과 선택 서비스가 응답하면
stackbox가 다시 `ready`가 됩니다. 복원한 stackbox도 이 방법으로 상태를 갱신할 수 있습니다.

```bash
msb stop dev && msb start dev   # 종료는 시간이 걸리므로 stop/start를 권장
stackbox settle dev
```

`msb restart`는 종료를 10초만 기다리고 `stackbox-identity`의 dummy 인터페이스가 사라진 상태로
부팅할 수 있습니다. 그때는 `msb stop`이 끝난 뒤 `msb start`로 다시 시작하세요.

## 고정 게스트 정체성

복원한 VM은 새 샌드박스 이름으로 부팅하고, msb가 매번 시작할 때 고르는 eth0 주소(`172.16.x.y/30`)도
달라집니다. 일반 재시작에서도 주소가 바뀔 수 있습니다. 배포된 OpenStack은 MariaDB·RabbitMQ·서비스
bind 주소와 RabbitMQ 노드 이름(`rabbit@<hostname>`), nova-compute·Neutron agent의 host 이름에 묶여
있으므로, 이 값이 바뀌면 동작하지 않습니다.

그래서 새 stackbox는 모두 같은 게스트 정체성을 가집니다.

- hostname `stackbox-aio`
- dummy 인터페이스 `stackbox0`의 서비스 주소 `10.254.0.2/24`. Kolla의 `network_interface`,
  `api_interface`, `kolla_internal_vip_address`가 이 주소를 사용합니다.
- eth0으로 들어오는 published 포트(5000·9292·8780·80·9696·8774)는 nftables DNAT로 `stackbox0`에 전달합니다.

`stackbox-identity.service`가 부팅할 때마다 `/etc/stackbox/identity.conf`를 읽어 위 설정을 적용하고,
`stackbox-hostname.service`가 hostname을 서비스 주소로 매핑한 뒤 Docker가 시작합니다.
이 설정은 stackbox마다 독립된 VM 안에서만 쓰이므로 stackbox끼리 충돌하지 않습니다.

이 기능 이전에 만든 stackbox는 eth0 주소로 배포되어 있어 얼릴 수 없습니다(`409`).
새 stackbox를 만들고 배포한 뒤 그 stackbox를 얼리세요.

## 얼리기

1. VM을 정상 종료합니다. MariaDB와 RabbitMQ가 깨끗하게 닫힙니다.
2. root disk와 Docker·containerd 디스크를 disk 스냅샷으로 저장합니다(msb 그룹 `stackbox-snap-<id>`).
3. VM을 다시 시작하고, 배포된 stackbox이면 Keystone·Glance와 선택 서비스(Neutron agent,
   nova-compute)가 다시 응답할 때까지 기다립니다.

job은 링크 끄기 → compute 정지 → control 정지 → VM별 캡처(i/N) → control 시작 → compute 시작·링크 켜기
→ 서비스 확인처럼 단계를 하나씩 기록하고, 포탈은 Stackbox 상세의 얼리기 배너, 작업 화면, `생성 중`인
스냅샷 행에 현재 단계와 경과 시간을 보여줍니다. 원격 호스트에서도 stackbox-agent가 단계를 전달합니다.

원본 stackbox는 이 동안 몇 분간 응답하지 않습니다. 상태는 그대로 유지되고, 실패는 job과 스냅샷에 기록됩니다.
캡처는 성공했지만 원본이 다시 시작되지 않으면 스냅샷은 `ready`, job은 `failed`가 됩니다.
메모리까지 저장하는 full 스냅샷은 macOS(HVF)에서 msb가 지원하지 않아 사용하지 않습니다.

`node`를 주면 VM 하나만 템플릿으로 저장합니다. `compute-N`은 compute 템플릿(그 compute만 멈춤),
`control`은 control 템플릿입니다. control 템플릿은 stackbox 전체처럼 compute → control 순서로 멈추고
control → compute 순서로 켜지만 control VM의 디스크만 저장합니다. 이 템플릿으로 복원한 stackbox는
control VM 하나로 시작해 원본 compute의 OpenStack 등록을 지운 뒤 `ready`가 되고, compute를 추가해
클러스터를 조립합니다(README의 "Control 템플릿으로 클러스터 조립").

## 불러오기

`POST /stackboxes`에 `snapshot_id`를 지정하면 `restore` job이 실행됩니다. 서비스 목록과 VM 크기는
스냅샷의 계획을 사용하므로 `components`는 생략합니다. 메모리·포트 예약은 일반 생성과 같습니다.

1. 스냅샷에서 `<id>` VM을 콜드부팅하고 새 stackbox의 포트를 publish합니다.
2. 복원한 VM에는 stackbox 라벨이 없으므로 `stackbox`·`stackbox.role` 라벨을 저장합니다(`next_start`, 재시작 없음).
   all-in-one VM은 이름(`<stackbox>`, 이전 버전은 `<stackbox>-all-in-one`)으로 stackbox를 판단하므로 원본 stackbox에 섞이지 않습니다.
3. 배포된 스냅샷이면 Keystone 카탈로그의 public endpoint를 원본의 `host:port`에서 새 stackbox의
   `host:port`로 바꾸고, `/etc/kolla/globals.yml`도 같이 고친 뒤 서비스 응답을 기다립니다.

복원하면 compute VM이 다시 올라올 때 nova-compute가 DB에서 실행 중이던 인스턴스를 스스로 다시 켭니다.
모든 stackbox의 `/etc/kolla/config/nova/nova-compute.conf`에 `[DEFAULT] resume_guests_state_on_host_boot = True`가
들어가므로 수동 `server start`는 필요 없습니다. 이 옵션이 없으면 Nova 기본값(False)대로 인스턴스가 SHUTOFF로 남습니다.

배포 전에 얼린 스냅샷은 `created` 상태로 복원되며 일반 stackbox처럼 배포합니다.
복원한 stackbox도 다시 배포할 수 있습니다.

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

OpenStack 릴리스는 label(`stackbox.snapshot.openstack-release`)로만 기록하고 복원을 막지는 않습니다.
스냅샷은 자기 Kolla 설치를 그대로 담기 때문입니다. 다만 서로 다른 릴리스에서 같은 stackbox를
다시 배포하면 서비스 이미지가 섞일 수 있으니, 복원 후에는 재배포 대신 서비스 확인을 쓰세요.

## 내보내기와 가져오기

내보내기는 golden image까지 포함한 self-contained 아카이브(tar+zstd)를 씁니다. 기본 경로는
`<state dir>/exports/<id>.msb`입니다. 경로에 폴더를 주면 그 안에 `<id>.msb`로 저장하고,
기존 파일은 덮어쓰지 않습니다. 완료될 때까지 `<path>.partial`에 쓴 뒤 이름을 바꿉니다.

스냅샷 메타데이터(서비스 목록, 배포 여부, 원본 public 포트, OpenStack 릴리스, CPU 아키텍처, 메모)는
msb 스냅샷 label로 저장되며 아카이브 안에 함께 들어갑니다. 가져오기는 이 label로 스냅샷을 다시
등록하고, label이 없거나 아키텍처가 다른 아카이브는 거절하고 정리합니다. ID를 생략하면 파일 이름을
사용합니다. 경로는 모두 stackboxd 호스트의 절대 경로입니다.

S3 버킷으로 옮기는 방법은 [S3 버킷](#s3-버킷)을 참고하세요.

내보내기와 가져오기는 job이 아니라 스냅샷 상태(`exporting`, `importing`)로 진행됩니다.
데몬이 중간에 종료되었을 때 다시 시도할 수 있는 상태로 바꿉니다.
스냅샷 삭제는 내보낸 파일을 지우지 않습니다. `msb snapshot rm`으로 디스크를 먼저 지웠다면 호스트
에이전트가 이미 없는 스냅샷으로 보고 삭제를 성공으로 처리하므로, 기록도 함께 지워집니다.
스냅샷을 보관하던 호스트가 없어졌거나 접속되지 않으면 `DELETE /snapshots/{id}?forget=true`
(`stackbox snapshot delete <id> --forget`, 포탈의 **기록만 삭제**)로 coordinator 기록만 지울 수 있습니다.
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

### 메타데이터 파일과 index.json

아카이브 안의 label을 읽으려면 GiB 단위로 내려받아야 하므로, 내보내기는 `<이름>.msb`를 다 올린
**뒤에** 옆에 `<이름>.json`을 씁니다. 이 파일이 있으면 업로드가 끝난 것입니다.

```json
{"schema": 1, "key": "stackbox/ctl.msb", "size_bytes": 2147483648, "sha256": "<hex>",
 "snapshot_id": "ctl", "release": "2025.1", "role": "control", "node": "control",
 "components": ["keystone", "nova", "neutron"], "computes": 0, "arch": "aarch64",
 "platform": "macos", "deployed": true, "source_stackbox": "dev", "note": null,
 "created_at": 1767225600, "exported_at": 1767229200}
```

`role`은 `control`/`compute` 템플릿이면 있고 stackbox 스냅샷이면 `null`입니다. `sha256`은 올리면서
계산하므로 아카이브를 다시 읽지 않습니다. 그다음 prefix의 `index.json`
(`{"schema":1,"generated_at":…,"entries":[메타데이터…]}`, 최신 내보내기순)에 항목을 더합니다.
index는 메타데이터 파일에서 언제든 다시 만들 수 있는 캐시입니다. 버킷 목록(`GET /buckets/{id}/objects`,
가져오기 대화상자)은 index를 읽되, 크기가 다르거나 index에 없는 아카이브는 메타데이터 파일을 직접
읽고, index가 없거나 깨졌거나 뒤처졌으면 다시 씁니다(읽기 전용 키면 경고만). 여러 coordinator가
동시에 써서 항목이 빠져도 다음 목록에서 채워집니다. `POST /buckets/{id}/index`나
`stackbox snapshot bucket <id> --rebuild-index`는 메타데이터 파일만으로 index를 새로 씁니다.
메타데이터 파일이 없는 예전 아카이브는 "메타데이터 없음"으로 보이고 그대로 가져올 수 있습니다.
`<prefix>index.msb`처럼 메타데이터 파일이 index와 겹치는 키로는 내보낼 수 없습니다.

가져오기는 메타데이터 파일이 있으면 내려받은 아카이브의 SHA-256이 같아야 등록하고, 다르면
실패합니다. 기록은 아카이브 안의 label로 만들며, label의 릴리스나 역할이 메타데이터 파일과
다르면 등록을 지우고 실패합니다. stackbox는 버킷의 객체를 지우지 않으므로, 버킷에서 아카이브를 직접
지웠다면 메타데이터 파일도 함께 지우세요(남아도 아카이브가 없으면 목록과 index에서 빠집니다).

S3 호환 서버마다 다른 동작을 피하려고 다음처럼 요청합니다.

- SigV4 서명, 본문은 SHA-256으로 서명합니다(`aws-chunked`·`x-amz-checksum-*` 헤더 없음).
- part는 마지막을 빼고 모두 같은 크기입니다(최소 64 MiB, 10,000개 안에 들어가게). R2가 요구합니다.
- 실패한 업로드는 abort합니다. abort하지 못했거나 데몬이 업로드 중에 멈췄다면 업로드 ID를
  기록해 두었다가 다음 시작 때 abort하므로, 미완료 part가 요금으로 남지 않습니다.
- redirect를 따라가지 않습니다. 리전이 틀리면 서버 오류 메시지가 그대로 스냅샷 오류에 남습니다.

버킷 등록을 해제해도 버킷에 올린 아카이브는 남습니다. 진행 중인 전송이 쓰는 버킷은 수정하거나
해제할 수 없습니다(`409`). 실제 서비스로 클라이언트를 확인하려면
`STACKBOX_S3_BUCKET`, `STACKBOX_S3_ACCESS_KEY`, `STACKBOX_S3_SECRET_KEY`(필요하면 `STACKBOX_S3_ENDPOINT`,
`STACKBOX_S3_REGION`, `STACKBOX_S3_PREFIX`, `STACKBOX_S3_PATH_STYLE`, `STACKBOX_S3_CA_FILE`)를 지정하고
`cargo test s3_live -- --ignored`를 실행합니다.

## 다른 호스트로 복사

스냅샷 원본은 얼린 호스트(가져온 스냅샷은 로컬 호스트)에 있습니다. 단일 VM 스냅샷(all-in-one stackbox,
control 템플릿, compute 템플릿)으로 다른 호스트에 stackbox를 만들거나 compute를 붙이면, job이 먼저
그 호스트에 스냅샷 복사본을 둡니다. 로컬 호스트도 원격 호스트에서 얼린 스냅샷에는 복사 대상의
하나입니다. 다음 사용부터는 호스트에 남은 복사본을 그대로 씁니다. 아키텍처는 대상 호스트의 것과
비교합니다. 다중 VM stackbox 전체 스냅샷은 얼린 호스트에서만 복원합니다.

원격 호스트에서 얼리기 전에, stackbox가 쓰던 실행기가 호스트의 현재 실행기와 다르면 배포처럼 현재
실행기로 바꿉니다. 현재 실행기가 이 coordinator와 다른 빌드면 `409`로 거절하니 Hosts에서 다시
설치하세요.

1. 호스트의 stackbox-agent에 복사본이 아직 있는지 묻습니다(`FindSnapshot`). agent는 자기가 불러온
   복사본을 coordinator별로 `cache.json`에 기록하고, msb에 그 스냅샷이 남아 있을 때만 있다고 답합니다.
2. 없으면 버킷 아카이브를 찾습니다. 마지막 내보내기가 버킷이었거나 버킷에서 가져온 스냅샷, 또는
   등록된 버킷의 메타데이터 파일이 같은 템플릿(스냅샷 ID·역할·VM·릴리스·서비스·아키텍처)을 가리키면
   호스트가 직접 내려받습니다. coordinator는 그 객체 하나만 6시간 동안 읽을 수 있는 presigned GET
   URL을 보내고, 버킷의 secret key는 vault 밖으로 나가지 않습니다. URL은 agent 연결(SSH, 접속해 오는
   agent의 TLS 소켓) 안으로만 전달되고 어디에도 저장되지 않습니다. 사설 CA는 버킷의 `ca_pem`을 함께
   보냅니다. agent는 메타데이터 파일의 크기와 SHA-256이 맞는지 확인합니다.
3. 버킷 아카이브가 없거나, 호스트가 버킷에 닿지 못하거나, 받은 내용의 SHA-256이 다르면 job 로그에
   이유를 남기고, coordinator가 원본을 내보낸 아카이브를 agent 연결로 직접 보냅니다(셸 입력과 같은
   경로, 96 KiB씩). agent는 내보낸 아카이브의 크기·SHA-256과 비교합니다. 원본이 원격 호스트에
   있으면 그 호스트가 먼저 아카이브를 만들고(`ExportArchive`), 셸 출력과 같은 경로로 coordinator에
   보냅니다(`SendArchive`). coordinator는 크기·SHA-256을 확인합니다. 파일이나 버킷으로 내보낼 때도
   같습니다.
4. 확인된 아카이브만 불러오고(`ImportArchive`), 스테이징 파일은 지웁니다.

진행 상황은 job 로그에 나옵니다(`host edge downloads ... from bucket ...`, `streamed 256 of 1024 MiB
to host edge`). 같은 스냅샷을 같은 호스트로 동시에 복사하는 작업은 차례로 실행되고, 뒤의 작업은
앞의 복사본을 씁니다. 스냅샷 기록의 `copies`가 복사본이 있는 호스트(`host_id`), 그 호스트의 msb
reference, 경로(`via`: `bucket`, `stream`, 또는 이미 있던 `found`)를 보여줍니다.

`DELETE /snapshots/{id}/copies/{host_id}`(`stackbox snapshot uncache <id> --host <host>`, 포탈 스냅샷 카드의
호스트 칩)는 그 호스트의 복사본만 지웁니다. 스냅샷을 삭제하면 원본 호스트의 디스크를 지운 뒤 복사본도
지우고, 접속되지 않는 호스트의 복사본은 응답의 `copies_left`에 남깁니다(기록은 삭제됨). 호스트
등록을 해제하면 그 호스트의 복사본 기록만 지웁니다. 원본 호스트의 등록을 해제한 스냅샷은 기록이
남지만, 그 호스트를 다시 등록할 때까지 내보내거나 새로 복사할 수 없고(`409`), 이미 복사본이 있는
호스트에서만 복원됩니다. 이때 삭제하면 원본은 지우지 않고 기록과 다른 호스트의 복사본만 지웁니다.

복사에는 이 기능을 아는 stackbox-agent가 필요합니다. agent의 `Hello` 응답에
`supports_template_transfer`가 없으면 `409`와 함께 "the stackbox-agent on host '...' predates template
transfers"로 거절하고, 호스트에는 새 요청을 보내지 않습니다. SSH 호스트는 Hosts에서 실행기를 다시
설치하고, 접속해 오는 호스트는 stackbox-agent를 새 버전으로 바꿔 다시 연결하세요. 프로토콜 버전(`1`)은
그대로이며, 새 작업은 기존 작업 옆에 추가되었습니다.

## API

| Method | Path (`/api/v1` 기준) | 반환 |
|---|---|---|
| POST | `/stackboxes/{id}/snapshots` | `{"id","note"}` → snapshot job (202) |
| GET | `/snapshots`, `/snapshots/{id}` | 스냅샷 목록 / 상세 |
| DELETE | `/snapshots/{id}` | 스냅샷과 다른 호스트의 복사본 삭제 (`?forget=true`: 기록만 삭제), 남은 복사본은 `copies_left` |
| DELETE | `/snapshots/{id}/copies/{host_id}` | 그 호스트의 복사본만 삭제 → 갱신된 스냅샷 |
| POST | `/snapshots/{id}/export` | `{"path"}` 또는 `{"bucket","key"}` 선택 → 내보내기 시작 (202) |
| POST | `/snapshots/import` | `{"path","id"}` 또는 `{"bucket","key","id"}` → 가져오기 시작 (202) |
| GET / POST | `/buckets` | S3 버킷 목록 / 등록 (secret은 응답에 없음) |
| GET / PUT / DELETE | `/buckets/{id}` | 버킷 상세 / 수정 / 등록 해제 |
| POST | `/buckets/test-connection`, `/buckets/{id}/check` | 저장 전 / 등록된 버킷 연결 확인 |
| GET | `/buckets/{id}/objects` | prefix 아래의 `.msb` 객체 (최신순, `metadata`는 메타데이터 파일의 내용 또는 `null`) |
| POST | `/buckets/{id}/index` | 메타데이터 파일에서 `index.json`을 다시 써서 반환 |
| POST | `/stackboxes` | `{"id","snapshot_id",...}` → restore job (202) |
| POST | `/stackboxes/{id}/settle` | 서비스 응답 확인 job (배포 재실행 없음) |

스냅샷 상태는 `creating`, `ready`, `exporting`, `importing`, `deleting`, `failed`입니다.
