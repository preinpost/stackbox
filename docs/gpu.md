# GPU(가상 장치) 연결

farm에는 실제 GPU가 없습니다. 그래도 Nova가 GPU를 스케줄링하는 경로는 시험할 수 있도록
farm은 **가상 장치** 두 가지를 제공합니다. 어느 쪽도 연산을 하지 않습니다. 인스턴스는
"GPU를 가진 것처럼" 예약되고 부팅되며, 게스트가 보는 장치는 스케줄링 계약을 확인하기 위한
것입니다.

| 모드 | Nova에 보이는 것 | 필요한 릴리스 | 필요한 커널 |
| --- | --- | --- | --- |
| `simulated` | compute마다 가짜 GPU 장치별 child resource provider의 표준 `VGPU` 인벤토리와 요청한 `CUSTOM_*` trait | OpenStack 2025.1 이상 | 기본(스톡) 커널 |
| `mtty` | 커널 샘플 `mtty` mediated device의 `VGPU` 인벤토리 | OpenStack 2025.1 이상(2026.1 이하는 farm이 Nova에 넣는 백포트 필요) | farm 기본 커널(VFIO/mdev 내장) |

`simulated`는 일반적인 vGPU 배포에서 Nova가 만드는 것과 같은 모양으로 Placement에
`VGPU`를 광고합니다. compute의 루트 resource provider 아래에 가짜 GPU 장치마다 child
resource provider를 만들고, 그 위에 표준 `VGPU` 인벤토리와 trait를 넣습니다. flavor가
`resources:VGPU=1`과 `trait:CUSTOM_...=required`를 요청하면 Placement가 그 child
provider에서 1개를 예약하고, 게스트에는 아무 장치도 생기지 않습니다. 표준 리소스
클래스로 스케줄·할당되는지 시험하는 것이 목적입니다.

`mtty`는 게스트 커널의 `mtty` mdev를 libvirt가 vGPU로 붙입니다. 게스트 `lspci`에는
vendor `4348`, device `3253`인 시리얼 컨트롤러가 보입니다. 업스트림 Nova는 2026.2에서야
`mtty`를 알게 되었으므로, 2026.1 이하 farm은 그 커밋을 백포트해 컨테이너 안의 Nova에
넣습니다([아래](#mtty-백포트-nova-20261-이하)).

## `simulated`의 provider 모양

Nova의 `provider_config`(Kolla의 `nova_cell_compute_provider_config`)는 `CUSTOM_*`
클래스만 받으므로 `VGPU`를 거기로 광고할 수 없습니다. farm은 대신 배포 뒤 Placement
API로 provider를 직접 만듭니다.

| 항목 | 값 |
| --- | --- |
| 이름 | `<compute host>_pci_0000_<bus>_<slot>_<func>` (예: `farm-compute-1_pci_0000_84_00_0`) |
| 부모 | 그 compute의 루트 resource provider |
| 인벤토리 | `VGPU`(또는 `gpu.resource_class`) — `total`은 요청한 수량을 장치 수로 나눈 몫 |
| trait | 요청한 `CUSTOM_*` trait (기본 `CUSTOM_FARM_VGPU`) |
| flavor | `farm.vgpu` — `resources:<RC>=1` + 각 trait `=required` |

이름 규칙은 장식이 아닙니다. 일반적인 vGPU 배포에서 Nova의 libvirt 드라이버가 물리 GPU의
PCI 주소로 child resource provider를 만들 때 쓰는 이름이 `<hypervisor_hostname>_pci_...`
이므로, farm도 같은 모양을 씁니다. bus 값은 farm이 정한 가짜 주소입니다(장치 `i`마다
`0x84 + i`). farm의 compute 제거 경로는 `farm-compute-`로 시작하는 provider를 지우므로,
이 이름은 farm이 만든 compute의 provider로 함께 정리됩니다.

farm은 `enabled_mdev_types`를 비워 두므로 Nova의 libvirt 드라이버는 이 provider를 만들지도
지우지도 않습니다. 대신 **farm이 멱등적으로 다시 만듭니다**: 배포의 검증 단계(따라서 `farm
deploy --from <단계>` 재실행도), compute 추가, 그리고 control 템플릿에서 복원해 compute를
다시 붙이는 경로입니다. 이미 있으면 인벤토리와 trait만 갱신하고, 없으면 부모 아래에 만듭니다.
farm 전체 스냅샷을 복원하면 Placement 데이터베이스가 control VM과 함께 돌아오므로 다시 만들
필요가 없습니다.

요청으로 계약을 바꿀 수 있습니다(API·CLI, `simulated` 전용).

| 필드 | 기본 | 뜻 |
| --- | --- | --- |
| `gpu.count` | 1 | compute마다 광고할 VGPU 수(1~24) |
| `gpu.devices` | 1 | compute당 가짜 GPU 장치 수(1~24, `count` 이하). 장치마다 child provider가 하나씩 생기고 수량이 나뉩니다 |
| `gpu.traits` | `["CUSTOM_FARM_VGPU"]` | flavor가 `=required`로 요구할 trait. `CUSTOM_[A-Z0-9_]+`만, 빈 배열이면 trait 없음 |
| `gpu.resource_class` | `VGPU` | flavor가 요청할 클래스. `VGPU`, `PGPU`, `CUSTOM_*` |

예를 들어 `"gpu":{"mode":"simulated","count":6,"devices":2,"traits":["CUSTOM_ALLOCATED"]}`는
장치 2개에 `VGPU`를 3개씩 광고하고, flavor는 `resources:VGPU=1`,
`trait:CUSTOM_ALLOCATED=required`를 요구합니다. 장치가 2개면 인스턴스의 할당이 compute가
아니라 카드 하나에 붙는지도 함께 시험할 수 있습니다. `gpu.resource_class`를 `PGPU`로 바꾸면 "표준
클래스가 아닌 다른 클래스" 계약도 같은 경로로 시험됩니다.

`mtty`는 이 세 필드를 받지 않습니다. 그 provider는 Nova가 mediated device를 보고 직접
만들고 관리하므로, farm이 클래스나 trait를 바꿀 수 없습니다. 요청하면 `400`입니다.

## 요구 사항

- farm의 컴포넌트에 `nova`가 있어야 하고, compute VM이 1대 이상이어야 합니다.
  (모든 farm이 control VM + compute 1대 이상으로 만들어지므로 보통은 그대로입니다.)
- 두 모드 모두 OpenStack 2025.1 이상만 받습니다. farm은 시험을 마친 릴리스에서만 이 경로를
  켭니다. `simulated`가 쓰는 Placement microversion은 1.14(provider의 부모를 지정하는 버전)이고,
  이는 2025.1보다 훨씬 앞선 릴리스부터 있습니다. `mtty`는 2026.2부터 업스트림 Nova가 직접
  지원하고, 2025.1·2025.2·2026.1은 farm이 배포 때 컨테이너 안의 Nova에 백포트를 넣습니다
  ([아래](#mtty-백포트-nova-20261-이하)). 2024.x는 이 두 경로를 시험하지 않았으므로 farm이 받지
  않습니다.
- GPU 수량은 compute마다 1~24개이고, `simulated`의 `gpu.devices`는 그 수량 이하입니다.
- `mtty`는 farm 커널이 설치·검증된 **호스트**에서만 됩니다(아래). farm은 compute VM이 뜨는
  호스트마다 그 호스트의 점검 결과(`inspection.mtty_available`)를 봅니다. compute가 없는 control
  VM만 도는 호스트는 farm 커널이 없어도 됩니다.

## farm 커널(`mtty`)

microsandbox의 기본 libkrunfw에는 VFIO·mdev가 없습니다. farm은 VFIO·mdev·mtty를 넣어 빌드한
커널을 **기본 게스트 커널**로 씁니다. `task dev`가 커널을 빌드하고, 호스트 준비가 그 호스트의
`~/.local/share/farm/libkrunfw/<id>/`에 설치한 뒤 farm의 모든 VM(검증 VM 포함)을 그 커널로
부팅합니다. 사용자가 환경변수를 줄 필요는 없습니다. `~/.microsandbox`는 그대로 둡니다.

```bash
task dev                  # 커널 빌드(실패해도 계속) + 데몬 + 포탈
farm host local           # 이 머신 준비: msb, farm 커널, 검증 VM
farm host list            # local Local Ready · custom kernel …/libkrunfw/<id>/… · kernel 6.12.113 · VFIO · mdev · mtty-1×24
```

커널은 `scripts/libkrunfw/fragments/`의 조각으로 빌드하며, 조각을 더하는 방법은
[docs/libkrunfw.md](libkrunfw.md)에 있습니다. Docker가 없으면 `task dev`는 커널 빌드를 건너뛰고
farm은 stock 커널로 동작합니다. 그때 `farm host list`에는 `(mtty)`가 붙지 않고 mtty farm은
게이트에서 거절됩니다. 커널을 직접 빌드하려면 `task libkrunfw`(이 머신) 또는
`task libkrunfw:all`(aarch64·x86_64)을 씁니다.

farm은 설치된 커널의 `manifest.txt`에 적힌 `msb`가 그 호스트의 msb와 같을 때만 씁니다. 다르면
stock 커널로 부팅하고 `farm host list`와 호스트 정보의 `custom_kernel_reason`에 이유를 남깁니다.
호스트 준비가 msb를 먼저 맞추므로, 이전 msb로 만든 커널은 다음 준비·재시작 뒤부터 쓰입니다.

경로를 직접 지정하던 `FARM_LIBKRUNFW_PATH`는 **최우선 override**로 남아 있습니다. 저장소 밖에서
빌드한 커널을 한 프로세스에서만 시험할 때 씁니다. `FARM_LIBKRUNFW_DIR`은 커널 산출물 디렉터리를
바꿉니다(기본값은 실행 파일 위의 `target/libkrunfw`). farm은 이 경로와
`~/.microsandbox/bin/msb`(`MSB_PATH`로 변경) 짝을 SDK에 직접 넣습니다.

호스트 준비는 커널을 지우지 않습니다. msb 설치 스크립트는 `~/.microsandbox/{bin,lib}`만 쓰고,
farm 커널은 `~/.local/share/farm/libkrunfw`에 있습니다. override가 `~/.microsandbox/lib` 안을
가리키는 경우에만 farm은 msb 재설치를 건너뜁니다(설치되어 있지 않다면 먼저 msb를 손으로
설치해야 합니다).

`farm host local` 준비 단계는 검증 VM을 그 커널로 부팅하면서 게스트 커널의 GPU 스택을 읽습니다.
결과는 호스트 정보의 `custom_kernel`(경로), `mtty_available`(검증 여부), `gpu_kernel`(아래)로
API와 포탈 Hosts 체크리스트, `farm host list`에 나옵니다. `mtty` farm은 이 검증이 통과한
호스트에서만 만들 수 있습니다.

검증 여부는 **그 farm의 compute VM이 뜨는 호스트마다** 판단합니다. coordinator 자신의 머신에
커널이 없어도(준비를 하지 않았어도) `mtty` farm을 만들 수 있습니다. 예를 들어 macOS에서 돌리는
coordinator가 SSH 호스트 `dev-01`에 farm을 만들면 `dev-01`의 `mtty_available`만 봅니다.
반대로 대상 호스트가 검증되지 않았으면 오류가 그 호스트의 id와 저장된 커널 정보
(`custom_kernel`, `custom_kernel_reason`, `gpu_kernel`)를 들어 왜 안 되는지 말하고, 그
호스트에서 `farm host prepare <id>`(또는 포탈 Hosts의 환경 준비)를 하라고 안내합니다.

계획 단계는 호스트와 무관합니다. `farm plan --gpu mtty`, `POST /api/v1/plans`(그리고
`POST /api/v1/releases/plan`)는 릴리스가 2025.1 이상인지, `nova`가 있고 compute VM이 있는지,
수량이 1~24인지만 확인합니다. 커널이 필요한지는 farm을 실제로 만들 때(`POST /farms`,
`POST /releases/farms`, compute 추가) 배치 대상 호스트의 점검 결과로만 봅니다.

`gpu_kernel`은 검증 VM의 게스트 커널에서 읽은 구조화된 값입니다. farm 커널은 VFIO·mdev를
모듈이 아니라 내장(built-in)으로 넣으므로, 아래 `true`는 "모듈을 적재했다"가 아니라 "커널 기능이
활성 상태로 올라왔다"는 뜻입니다.

```json
"gpu_kernel": {
  "kernel": "custom",
  "kernel_id": "72f012b2c2c8c33c",
  "vfio": true,
  "vfio_mdev": true,
  "mtty_registered": true,
  "mdev_types": [{"name": "mtty-1", "available_instances": 24}],
  "kernel_release": "6.12.113"
}
```

| 필드 | 읽는 곳 | 뜻 |
| --- | --- | --- |
| `kernel` | 선택된 커널 | `custom`이면 farm 커널, `stock`이면 microsandbox 기본 커널 |
| `kernel_id` | 커널 경로 | `~/.local/share/farm/libkrunfw/<id>`의 `id`; override면 없음 |
| `reason` | inspection | 설치된 커널을 쓰지 않고 stock으로 부팅한 이유 |
| `vfio` | `/dev/vfio/vfio` | VFIO 컨테이너가 올라옴 |
| `vfio_mdev` | `/sys/class/mdev_bus` | mdev 버스가 등록됨 |
| `mtty_registered` | `/sys/class/mdev_bus/mtty` | 커널 샘플 mtty 부모 장치가 등록됨 |
| `mdev_types[]` | `mdev_supported_types/*` | 타입별 `available_instances` |
| `kernel_release` | `uname -r` | 게스트 커널 릴리스 |

이 필드는 검증 VM을 부팅한 준비만 채웁니다. 구버전 coordinator가 저장한 state에는 없으므로
(`Option`, serde default) 포탈은 "환경 준비 후 확인"으로 표시하고, 준비를 다시 실행하면 채워집니다.
일반 점검(`check`)은 커널과 msb가 그대로일 때 마지막 검증 값을 유지합니다.

준비는 데몬이 호스트를 처음 만질 때 일어납니다. 데몬을 이미 띄웠다면 포털의 Hosts 페이지에서
준비하거나, 데몬을 잠시 내리고 `farm host local`을 실행한 뒤 다시 띄우면 됩니다. 검증 결과는
상태에 남으므로 커널과 msb가 그대로면 데몬을 다시 시작해도 유지됩니다.

SSH 원격 호스트도 준비 단계에서 coordinator가 그 호스트 아키텍처의 Linux `.so`를
`~/.local/share/farm/libkrunfw/<id>/`에 업로드하고 sha256을 확인합니다. 그 호스트의 agent가
설치된 커널로 검증 VM과 farm VM을 부팅합니다. `farm agent connect`로 붙는 dial-in 호스트는
coordinator가 파일을 밀어 넣을 수 없으므로 그 머신에 커널이 설치되어 있거나
`FARM_LIBKRUNFW_PATH`가 있어야 mtty farm을 받을 수 있습니다.

```console
$ farm host list
local   Local   Ready   · custom kernel /Users/me/.local/share/farm/libkrunfw/0123456789abcdef/libkrunfw.5.dylib · kernel 6.12.113 · VFIO · mdev · mtty-1×24
```

## 사용 예

### CLI

```bash
# 어떤 VM이 나오는지 미리 보기 (호스트의 커널과 무관하게 계산됩니다)
farm plan --components nova --gpu simulated --gpu-count 2
farm plan --components nova --gpu mtty

# simulated: 호스트에 커널 준비가 필요 없습니다.
farm create demo-gpu --host-id local --components nova --gpu simulated --gpu-count 2
farm deploy demo-gpu

# 계약을 직접 정하기: 가짜 GPU 장치 2개에 trait 2개, 다른 리소스 클래스
farm plan --components nova --gpu simulated --gpu-count 6 --gpu-devices 2 \
  --gpu-trait CUSTOM_MAAS --gpu-trait CUSTOM_SRIOV --gpu-resource-class PGPU

# mtty: 호스트를 한 번 준비하면 farm 커널이 설치되고 (mtty)가 표시됩니다.
farm host local
farm create demo-vgpu --host-id local --components nova --gpu mtty
farm deploy demo-vgpu

# API·포털은 `task dev`(또는 `task serve` + `task portal-dev`)로 띄웁니다.
# 준비 결과는 상태에 남아 데몬을 다시 시작해도 그대로 쓰입니다.
```

`--gpu`를 생략하면 GPU가 없는 farm입니다. compute를 여러 호스트에 두면 `mtty`는 그 모든
호스트가 farm 커널로 준비·검증되어 있어야 합니다. 로컬·SSH 호스트 모두 준비 단계에서 커널을
설치합니다.

### REST API

```bash
curl -sS http://127.0.0.1:10010/api/v1/plans \
  -H 'content-type: application/json' \
  -d '{"components":["nova"],"computes":1,"gpu":{"mode":"simulated","count":2}}'

# 릴리스 단위 생성: GPU farm은 템플릿을 쓰지 않고 클린 설치합니다.
curl -sS http://127.0.0.1:10010/api/v1/releases/farms \
  -H 'content-type: application/json' \
  -d '{"id":"demo-gpu","release":"2026.2","host_id":"local","components":["nova"],
       "computes":1,"gpu":{"mode":"mtty","count":1},"external_host":"localhost"}'
```

응답의 `plan.gpu`에 확정된 설정이 저장됩니다. `mode`만 주면 `count`는 1입니다.

## 배포가 하는 일

- `simulated`: Kolla 설정에는 GPU 관련 항목이 들어가지 않습니다. Nova 서비스가 올라온 뒤
  검증 단계에서 Placement API로 compute마다 가짜 GPU 장치별 child resource provider를
  만들고(있으면 갱신하고) 인벤토리·trait를 넣습니다. resource provider에는 trait 표에 있는
  이름만 넣을 수 있고 새 Placement에는 farm의 `CUSTOM_*` 이름이 없으므로, farm이 먼저
  `PUT /traits/<name>`으로 등록합니다(이미 있으면 그대로입니다). 이어서 flavor `farm.vgpu`를
  `resources:<RC>=1`과 trait `=required`로 맞춥니다. compute를 추가하면 그 compute의
  provider만 같은 방식으로 광고합니다.
- `mtty`: control VM의 `/etc/kolla/config/nova/nova-compute.conf`에 아래 절을 덧붙입니다(모든 farm이
  이 파일의 `[DEFAULT]` 절을 씁니다). 이 파일과 같은 배포에서, 2026.1 이하 릴리스에는 아래의 백포트 파일도 compute마다 만들어 둡니다. 둘은 함께
  들어가야 합니다. `enabled_mdev_types`만 있고 백포트가 없으면 nova-compute는
  `InvalidLibvirtMdevConfig`(또는 `mtty_mtty`를 본 뒤의 `AttributeError`)로 기동하지 못합니다.

  ```ini
  [devices]
  enabled_mdev_types = mtty-1

  [mdev_mtty-1]
  device_addresses = mtty_mtty
  ```

  Kolla가 이 파일을 nova-compute의 `nova.conf`에 병합합니다. 배포 전에 compute VM마다
  `mtty-1` 타입이 있는지와 수량을 확인합니다. `available_instances`는 남은 수이므로, 이미
  만들어진 mdev(`/sys/bus/mdev/devices`의 수)를 더한 총량이 요청한 수량보다 적을 때만
  실패합니다. 타입 자체가 없으면 farm 커널 없이 호스트가 준비되었다는 오류로 먼저 실패합니다. flavor는
  `farm.vgpu`(`resources:VGPU=1`)이고, 이미 있는 flavor의 속성은 배포할 때 다시 맞춥니다.
- 검증 단계에서 farm은 child provider의 존재·부모·인벤토리·trait, GPU flavor 인스턴스의
  `ACTIVE`, 인스턴스의 할당이 farm이 광고한 child provider에 붙었는지를 확인합니다. 이어서
  한 단계 큰 flavor(`resources:<RC>=<count+1>`)로 인스턴스를 하나 더 요청해 `NoValidHost`로
  거절되는지(= 재고가 실제로 스케줄러의 한계인지) 확인하고 그 인스턴스를 지웁니다.
  `mtty`는 compute의 libvirt 도메인에 `type='mdev'` hostdev가 붙었는지도 확인합니다.
  백포트를 쓰는 릴리스에서는 nova_compute 컨테이너의 두 파일 sha256이 호스트에서 만든 사본과
  같은지도 compute마다 확인합니다.

인스턴스는 보통처럼 만듭니다.

```bash
. /etc/kolla/admin-openrc.sh
openstack server create --flavor farm.vgpu --image cirros-farm --network <net> demo
openstack server show demo -f value -c status          # ACTIVE
openstack resource provider usage show $(openstack resource provider list \
  -f value -c uuid -c name | awk '$2 ~ /^farm-compute-1_pci_/{print $1}' | head -1)
```

## mtty 백포트 (Nova 2026.1 이하)

업스트림 Nova는 `f412fac1ae`("Add mtty support for testing fake mdevs")에서 `mtty` 부모
장치를 PCI 주소 없이 받아들이게 되었고, 그 커밋은 stable/2026.2에만 있습니다. farm은 그보다
오래된 릴리스의 Nova 파일에 같은 변경을 넣습니다. Kolla 이미지를 다시 빌드하지는 않습니다.

| 릴리스 | 이미지 | 넣는 패치 | 업스트림과 다른 점 |
| --- | --- | --- | --- |
| 2026.2 이상 | — | 없음 | 업스트림 코드가 그대로 씁니다 |
| 2026.1 | `nova-compute:2026.1-rocky-10` | `src/kolla/patches/nova-mtty-2026.1.diff` | 없음(서비스가 쓰는 두 파일만) |
| 2025.2 | `nova-compute:2025.2-rocky-10` | `src/kolla/patches/nova-mtty-2025.2.diff` | `ty.Optional[str]` 표기 |
| 2025.1 | `nova-compute:2025.1-rocky-9` | `src/kolla/patches/nova-mtty-2025.1.diff` | 위에 더해 메서드 삽입 위치와, 2026.2가 지운 privsep 분기의 가상 부모 거부 |

### 호스트에서 파일을 만드는 방법

이미지의 Nova 파일을 컨테이너 안의 파이썬이 아니라 **배포 전에 호스트에서** 바꿉니다.
nova_compute는 root 소유 venv 파일을 쓸 수 없는 `nova` 사용자로 뜨므로, 시작할 때 패치하면
PermissionError로 기동하지 못합니다(B4).

- `deploy` 단계 직전에 compute 호스트마다 `/etc/farm/prepare-nova-mtty.sh`를 실행합니다.
  스크립트는 이미지를 `docker pull`하고, 그 이미지를 `--user root`로 임시 컨테이너에 띄워
  `config.py`·`driver.py`를 작업 디렉터리에 복사해 패치한 뒤, 결과를 `/etc/farm/nova-mtty/<릴리스>/`
  아래에 씁니다. 원본 이미지는 바뀌지 않습니다.
- `globals.yml`에 두 파일을 읽기 전용 bind mount로 더합니다. Kolla의 기본 목록을 **대체하지
  않고** 잇습니다: `nova_compute_extra_volumes: "{{ default_extra_volumes + ['…/config.py:/var/lib/kolla/venv/lib/python3.X/site-packages/nova/virt/libvirt/config.py:ro', …] }}"`.
  nova_compute에만 붙습니다. venv는 `lib64`가 `lib`로 이어지는 심볼릭 링크라 실제 경로는
  `lib/python3.X`입니다(2025.1은 3.9, 2025.2·2026.1은 3.12).
- 패치는 줄 번호가 아니라 **텍스트로** 찾습니다. 옛 텍스트나 새 텍스트가 정확히 한 번 있는
  hunk만 적용하고, 어느 쪽도 찾지 못하면 그 자리에서 실패합니다. 파일은 모두 맞춰 보고
  `compile()`로 문법을 확인한 뒤에야 씁니다.
- 파일 내용이 바뀌면(패치나 이미지가 바뀌면) 스크립트를 다시 돌리면 됩니다. 다른 호스트의
  compute에는 control VM이 SSH 키로 스크립트를 밀어 넣습니다.

### 확인

검증 단계는 인스턴스를 만들기 전에 compute마다 nova_compute 컨테이너의 두 파일과 호스트의
사본을 `sha256sum`으로 비교합니다. 다르거나 컨테이너가 파일을 갖지 못했으면 여기서 실패합니다.
손으로 확인하려면:

```bash
ssh farm-compute-1 'docker exec nova_compute sha256sum /var/lib/kolla/venv/lib/python3.12/site-packages/nova/virt/libvirt/{config,driver}.py'
ssh farm-compute-1 'sha256sum /etc/farm/nova-mtty/2026.1/nova/virt/libvirt/{config,driver}.py'
```

### 소스와 라이선스

패치 파일 머리말에 업스트림 커밋 해시와 제목, 라이선스(Apache License 2.0, OpenStack Nova)를
적었습니다. 서비스가 쓰지 않는 업스트림 단위 테스트는 넣지 않았고, Python 표기와 2025.1의
privsep 분기만 farm이 손봤습니다. 패치 파일이 그 릴리스의 실제 소스에 적용되고 `py_compile`을
통과하는지는 네트워크가 필요한 테스트로 확인합니다(각 브랜치를 sparse clone).

```bash
cargo test the_mtty_backport_applies_to_the_upstream_branches -- --ignored --nocapture
```

네트워크 없이 돌리는 짝도 있습니다. 각 hunk의 패치 전 컨텍스트만 이어 붙인 가짜 소스에
`apply.py` 자신의 매처로 hunk를 얹어, 한 파일 안에서 앵커가 겹치거나 hunk가 서로를 덮지
않는지 봅니다.

```bash
cargo test the_mtty_backport_hunks_match_their_own_context_uniquely
```

### 백포트 갱신 절차

이미지가 바뀌어 패치가 맞지 않으면 farm의 검증이 실패합니다. 그때는:

1. 그 릴리스 브랜치의 새 소스에서 두 파일을 받아 hunk를 다시 만듭니다(패치 머리말과 farm이
   다른 점은 그대로).
2. 아래 두 테스트를 차례로 돌립니다. 첫째는 네트워크 없이, 둘째는 네트워크가 있는 곳에서
   릴리스 전에 반드시 실행합니다.

```bash
cargo test the_mtty_backport_hunks_match_their_own_context_uniquely
cargo test the_mtty_backport_applies_to_the_upstream_branches -- --ignored --nocapture
```

## 가짜 `nvidia-smi`와 부하 제어

farm은 GPU farm의 인스턴스에 **가짜 `nvidia-smi`**를 설치하고, 포탈에서 인스턴스별 부하를
조절합니다. 실제 GPU가 없으므로 값은 Nova 인스턴스 메타데이터에서 읽습니다. Telegraf
`inputs.nvidia_smi`나 `nvidia-smi --query-gpu=... --format=csv` 같은 일반적인 모니터링
경로를 그대로 시험하는 것이 목적이며, 어떤 연산도 하지 않습니다.

### 게스트 명령

| 명령 | 출력 |
| --- | --- |
| `nvidia-smi -q -x` | XML. DOCTYPE은 `nvsmi_device_v11.dtd`이고 Telegraf `inputs.nvidia_smi`가 파싱하는 v11 스키마입니다 |
| `nvidia-smi -q` | 짧은 텍스트 보고서 |
| `nvidia-smi --query-gpu=... --format=csv[,noheader][,nounits]` | 요청한 필드의 CSV. GPU가 여러 장이면 한 줄씩 |
| `nvidia-smi -L` | `GPU 0: farm-virtual-gpu (UUID: GPU-...)` |
| `nvidia-smi --version` | 드라이버·연산 API 버전 |
| `nvidia-smi` | 짧은 요약 |

지원 필드는 `index`, `name`, `uuid`, `serial`, `driver_version`, `cuda_version`,
`utilization.gpu`, `utilization.memory`, `utilization.encoder`, `utilization.decoder`,
`memory.total`, `memory.used`, `memory.free`, `temperature.gpu`, `power.draw`,
`power.limit`, `fan.speed`, `clocks.sm`, `clocks.gr`, `clocks.mem`, `clocks.video`,
`pstate`, `compute_mode`, `persistence_mode`, `display_mode`, `display_active`,
`gpu_bus_id`, `pci.bus_id`, `vbios_version`, `ecc.mode.current`,
`pcie.link.gen.current`, `pcie.link.width.current`입니다. 모르는 필드는 `N/A`입니다.
정수 필드는 정수로, `power.draw`와 `power.limit`는 항상 소수점이 있는 값으로 나옵니다.
모든 진단은 `/dev/null`로 보내므로 stdout과 stderr를 함께 읽는 Telegraf가 문서만 봅니다.

### 값의 출처: 인스턴스 메타데이터

farm은 포탈에서 받은 부하를 한 주기의 사용률 표본(최대 24개)으로 계산해 Nova 인스턴스
메타데이터에 씁니다. 게스트는 `169.254.169.254/openstack/latest/meta_data.json`을 2초
캐시로 읽고, 현재 초에 해당하는 표본을 고릅니다. Nova의 메타데이터 캐시 때문에 포탈에서
적용한 값이 게스트에 보이기까지 **최대 약 20초** 걸립니다(실측: simulated 8초, mtty 20초).

| 키 | 값 |
| --- | --- |
| `farm_gpu_count` | 이 인스턴스의 가상 GPU 수(1–24). farm이 flavor의 `resources:<class>`에서 정합니다 |
| `farm_gpu_<i>` | `v1;p:sim-8g;q:sine;n:10;x:90;d:60;m:30;u:...;id:GPU-...` |

`p`는 프로파일, `q`는 패턴, `n`·`x`는 최소·최대 사용률(%), `d`는 주기(초), `m`은 메모리
사용 비율(%), `u`는 표본 표, `id`는 결정적 UUIDv5(`GPU-...`)입니다. 값은 Nova의 255자
제한 안에 들어갑니다. `farm_gpu_*`가 없으면 게스트는 기본값(1장, 0%)을 보고하고,
`farm_gpu_count`가 `0`이면 실제 `nvidia-smi`처럼 `No devices were found`를 출력하고
종료 코드 6으로 끝납니다.

farm은 인스턴스에 부하를 처음 적용할 때 flavor의 GPU 수량으로 `farm_gpu_count`를 씁니다.
그 전에는 게스트가 기본 1장을 보고하므로, 여러 장짜리 flavor는 포탈에서 한 번 적용한 뒤부터
flavor 수량만큼 보입니다.

프로파일은 `sim-8g`(8 GiB, 150 W), `sim-16g`(16 GiB, 250 W), `sim-40g`(40 GiB, 350 W),
`sim-80g`(80 GiB, 400 W)입니다. 패턴은 `constant`, `sine`, `triangle`, `square`,
`sawtooth`, `random`, `spike`입니다. GPU가 여러 장이면 표를 GPU 인덱스만큼 돌려
위상차를 줍니다. 온도(30–85 °C), 전력(한계의 10–100%), 팬(20–100%), SM·그래픽 클럭,
pstate는 사용률에서 정수 연산으로 계산합니다. `random`은 같은 부하에 같은 표를 그리도록
결정적이므로, 포탈의 미리보기와 게스트가 같은 모양을 냅니다.

### 설치

- **cloud-init 이미지(Ubuntu 등)**: GPU farm의 `configure` 단계가
  `/etc/kolla/config/nova/vendordata.json`에 cloud-config를 씁니다. Kolla가 이 파일을
  nova_api·nova_metadata에 넣고, cloud-init이 user-data 없이 병합해
  `/usr/local/sbin/farm-gpu-install`을 실행합니다. farm이 만드는 모든 인스턴스에
  적용됩니다.
- **CirrOS 등 vendordata를 적용하지 않는 이미지**: 포탈 GPU 탭에서 **user-data 설치
  스크립트**를 내려받아 인스턴스에 넘깁니다. farm의 GPU 검증 인스턴스도 이 방식으로
  설치하고 콘솔에서 XML 출력을 확인합니다.

이 기능이 생기기 전에 만든 GPU farm은 vendordata 파일이 없습니다.
`farm deploy <id> --from configure`로 설정 단계를 다시 실행하면 파일이 생기고, 이어지는
배포가 nova_api·nova_metadata에 넣습니다. 이미 배포된 farm의 인스턴스에는 user-data
설치 스크립트를 쓰면 됩니다.

```bash
# 설치 스크립트 내려받기
curl -fsS http://127.0.0.1:10010/api/v1/farms/demo-gpu/gpu/user-data \
  -o farm-gpu-user-data.sh
openstack server create --flavor farm.vgpu --image cirros-farm --network demo \
  --user-data farm-gpu-user-data.sh demo-gpu-1
```

설치 스크립트는 `/dev/nvidia0`·`/proc/driver/nvidia/version`이 있거나 `lspci -n`에서 PCI vendor `10de`를 찾으면 설치하지 않습니다. 실제 GPU가
있는 이미지에서는 그 장치의 드라이버와 `nvidia-smi`를 그대로 둡니다.

게스트 스크립트는 `FARM_GPU_METADATA_URL`(기본 `http://169.254.169.254/...`),
`FARM_GPU_METADATA_FILE`(파일로 대체, 테스트용), `FARM_GPU_CACHE_DIR`(기본 `/run/farm-gpu`),
`FARM_GPU_CACHE_TTL`(기본 2초) 환경변수를 받습니다.

### Telegraf

`inputs.nvidia_smi`는 기본 `bin_path`가 `/usr/bin/nvidia-smi`이고 `-q -x`만 씁니다.
게스트에 다른 설정 없이 같은 measurement가 나옵니다.

```toml
[[inputs.nvidia_smi]]
  # bin_path = "/usr/bin/nvidia-smi"   # 기본값
  # timeout = "5s"

[[outputs.file]]
  files = ["stdout"]
  data_format = "influx"
```

### API와 포탈

GPU farm의 포탈 상세에는 **GPU 부하** 탭이 생깁니다. 인스턴스마다 프로파일·패턴·
최소·최대 사용률·주기·메모리 비율을 정하고, 같은 표를 그리는 스파크라인을 보면서
적용하거나 기본값으로 되돌릴 수 있습니다.

```bash
# 인스턴스 목록과 flavor의 GPU 리소스 클래스
curl -sS http://127.0.0.1:10010/api/v1/farms/demo-gpu/instances

# 부하 적용(예: sim-16g, 10–90% 사인, 60초 주기, 메모리 40%)
curl -sS -X PUT \
  http://127.0.0.1:10010/api/v1/farms/demo-gpu/instances/<server-uuid>/gpu-load \
  -H 'content-type: application/json' \
  -d '{"profile":"sim-16g","pattern":"sine","min":10,"max":90,"period_s":60,"memory_ratio":40}'

# 기본값으로 되돌리기
curl -sS -X DELETE \
  http://127.0.0.1:10010/api/v1/farms/demo-gpu/instances/<server-uuid>/gpu-load
```

`GET /farms/{id}/gpu/guest-script`는 가짜 `nvidia-smi` 원본을, `GET /farms/{id}/gpu/user-data`는
설치 스크립트를 돌려줍니다. GPU farm이 아니거나 Nova가 없는 farm은 `409`입니다. 부하
제어는 farm이 `ready`이고 진행 중인 작업이 없을 때만 됩니다. 인스턴스는 UUID 또는
유일한 이름으로 지정합니다.

## 한계

- **실제 GPU가 아닙니다.** 게스트가 장치를 봐도 연산할 수 없습니다. `simulated`는 게스트에
  장치 자체가 없습니다.
- 가짜 `nvidia-smi`는 **XML·CSV·목록·버전만** 흉내 냅니다. DCGM, NVML,
  `nvidia-smi --query-compute-apps`, 프로세스 목록, MIG, ECC 오류 주입은 없습니다. 값은
  메타데이터에서 오므로 실제 센서나 드라이버를 읽지 않습니다. 모니터링 에이전트가 이
  명령들만 쓰는지 먼저 확인하십시오.
- 가짜 `nvidia-smi`는 GPU farm의 모든 인스턴스에 설치됩니다. GPU flavor가 아닌 인스턴스는
  `farm_gpu_*` 메타데이터가 없으므로 기본값(1장, 0%)을 보고합니다. 부하를 적용하면
  flavor에 GPU 리소스가 없어도 1장으로 메타데이터가 기록됩니다.
- 두 모드 모두 OpenStack 2025.1 이상입니다. 그보다 오래된 farm은 GPU 없이 그대로 동작합니다.
- 2026.1 이하의 `mtty`는 **farm이 들고 가는 Nova 백포트**에 기댑니다(업스트림 백포트가
  아닙니다). 그 릴리스들은 각각 stable/unmaintained 브랜치라 버그픽스가 계속 들어오므로,
  이미지의 `driver.py`·`config.py` 문맥이 바뀌면 패치가 맞지 않을 수 있습니다. farm은 그때
  조용히 넘어가지 않고 컨테이너 기동과 검증을 실패시킵니다. 패치가 맞지 않으면
  `src/kolla/patches/nova-mtty-<릴리스>.diff`를 그 릴리스의 새 소스에 맞춰 고쳐야 합니다
  (위의 [mtty 백포트](#mtty-백포트-nova-20261-이하) 절).
- `simulated`의 인벤토리는 **드라이버가 만든 것이 아닙니다.** `nova hypervisor-show`의
  `VGPU` 사용량, compute의 자가 치유, PCI 정리 로직은 이 provider를 모릅니다. 호스트가 실제
  vGPU 능력을 갖는다는 신호를 인위적으로 만드는 셈이므로 시험용으로만 쓰십시오.
  `enabled_mdev_types`를 함께 켜면 드라이버가 같은 클래스를 자기 방식으로 다루려 하므로
  farm은 두 경로를 동시에 켜지 않습니다.
- compute를 제거할 때 그 compute의 child provider에 인스턴스 할당이 남아 있으면 Placement가
  삭제를 `409`로 거절합니다. 인스턴스를 먼저 지우고 compute를 제거하십시오.
- GPU farm은 릴리스 템플릿(스냅샷)으로 만들지 않습니다. 템플릿의 VM은 동결 당시 plan으로
  부팅하므로, GPU 설정은 클린 설치에서만 확실히 들어갑니다. 같은 이유로 GPU farm의 control
  VM은 control 템플릿으로 얼릴 수 없고(템플릿에는 compute도 GPU도 없습니다), 요청하면 `409`로
  거절합니다. farm 전체를 얼리는 것은 됩니다.
- `mtty`에서 `count`는 커널이 제공하는 mdev 수(최대 24)를 넘지 않는지 확인하는 값입니다.
  확인은 남은 수에 이미 만들어진 mdev를 더한 총량으로 하므로, 인스턴스가 장치를 쓰고 있어도
  같은 farm의 재배포가 실패하지 않습니다. flavor는 인스턴스마다 1개를 요청합니다.
- `mtty`의 mdev는 인스턴스를 지워도 사라지지 않습니다. Nova는 인스턴스 삭제 때 mdev를 지우지
  않고, 다음 생성에서 할당되지 않은 mdev를 먼저 재사용합니다(`nova/virt/libvirt/driver.py`:
  stable/2026.1 9225–9311, stable/2025.2 9086–9176, 2026.2 9359). 그래서 Placement의 `VGPU`
  사용량은 인스턴스 수를 따르지만 `/sys/bus/mdev/devices`의 수와 `available_instances`는
  생성 최대치까지 남습니다. 이는 설계이며 farm 결함이 아닙니다. mdev는 compute VM을 재부팅하면
  사라집니다.
- `mtty`에서 같은 compute에 mdev flavor 인스턴스를 **동시에** 둘 이상 만들면 한 인스턴스가
  `ERROR`(`MaxRetriesExceeded`, 원인: libvirt `mediated device … is in use by driver QEMU`)로
  끝날 수 있습니다. Nova는 mdev 선택을 `VGPU_RESOURCE_SEMAPHORE` 안에서 하지만, 선택한 mdev가
  libvirt 도메인에 기록되기 전 약 3초 동안은 다른 빌드가 그 mdev를 미할당으로 봅니다. 시험에서
  동시 생성 3회 중 2회가 이렇게 실패했습니다(순차 생성은 0회). 실패한 인스턴스는 지우고 다시
  만드십시오. 근본 수정은 Nova 업스트림 변경이 필요하므로 farm은 고치지 않습니다.
- compute VM을 추가하면 farm이 그 compute의 provider를 바로 광고하고, GPU flavor 인스턴스
  검증은 farm 전체 배포의 검증 단계에서만 수행합니다.
- 기존 farm은 상태 파일에 GPU 필드가 없어도 그대로 읽고 배포합니다(`GPU 없음`).

## 부록: 커널 경로를 직접 지정하기

`task` 없이 farm을 띄우거나, 저장소 밖에서 빌드한 커널을 시험하려면 경로만 직접 줍니다.
`MSB_PATH`는 생략해도 `~/.microsandbox/bin/msb`를 씁니다.

```bash
export FARM_LIBKRUNFW_PATH="$(sh scripts/libkrunfw-path.sh)"   # 없으면 먼저 빌드합니다
cargo run --release -- serve
```

경로는 OS와 아키텍처에 따라 `target/libkrunfw/<arch>/libkrunfw.<abi>.dylib`(macOS) 또는
`target/libkrunfw/<arch>/libkrunfw.so.<version>`(Linux)입니다. `scripts/libkrunfw-path.sh`는
`manifest.txt`에 적힌 이 머신용 산출물을 고르고, 이미 있으면 다시 빌드하지 않습니다(조각이나
리비전을 바꿨다면 `--build`). 이 override는 그 프로세스와 그 프로세스가 띄우는 로컬 agent에만
적용되며, SSH 호스트에는 설치된 커널이 쓰입니다.
