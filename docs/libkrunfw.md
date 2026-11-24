# 커스텀 게스트 커널(VFIO/mdev, Netfilter) 빌드

farm이 쓰는 msb 게스트 커널(libkrunfw)에는 모듈 로딩이 없고 VFIO·mdev도 들어 있지 않다.
그래서 Nova의 vGPU 경로(`[devices]/enabled_mdev_types = mtty-1`)를 GPU 없이 시험할 수 없다.
이 문서는 커널 설정 조각(`scripts/libkrunfw/fragments/`)을 얹어 커널을 직접 빌드하는 방법과,
새 기능이 필요할 때 조각을 하나 더하는 방법을 다룬다. 지금 들어 있는 조각은 셋이다.

- `vfio-mtty`: mdev 프레임워크와 커널 샘플 `mtty` 드라이버를 넣어 게스트 안에 `mtty` 부모
  장치와 `mtty-1` 타입을 만든다. libvirt가 mdev 장치를 만들 수 있는지까지 실제 GPU 없이 본다.
- `neutron-netfilter`: Neutron L3 agent가 라우터의 mangle 테이블에 쓰는 CONNMARK(연결 mark
  저장·복원)를 켠다. 없으면 `iptables-restore`가 그 규칙을 거부해 qrouter 네임스페이스가
  생겼다 사라지고, 테넌트 라우팅·플로팅 IP를 쓸 수 없다. 2025.1~2026.2의 레거시 L3가 같은
  규칙을 쓰므로 릴리스와 무관하게 필요하다.
- `pci-passthrough` (x86_64만): ACPI VIOT, MSI, virtio IOMMU와 USB xHCI(PCI)를 켜고, PCIe 포트/capability 지원을 위한 커널 옵션(PCIEPORTBUS)을 켠다. libkrun은 PCIe 루트 포트를 만들지 않고 통과시키는 endpoint를 bus 0에 직접 배치한다.
  PCI·IOMMU_SUPPORT·VFIO_PCI는 `vfio-mtty`가 이미 켜므로 이 조각은 그 밖의 설정만 담는다.

이 커널은 **시험용**이다(실제 GPU 대신 vGPU 경로만 확인한다). farm은 이 커널을 기본 게스트
커널로 쓴다. `task dev`가 커널을 빌드하고, 호스트를 준비할 때마다 그 호스트의
`~/.local/share/farm/libkrunfw/<id>/`에 설치한 뒤 그 커널로 VM을 부팅한다. `FARM_LIBKRUNFW_PATH`는
다른 곳에 둔 커널을 한 프로세스에서만 시험할 때 쓰는 override다. 저장소에는 바이너리를 두지
않는다. GPU 기능과 조건은 [docs/gpu.md](gpu.md)에 정리했다.

## 전제

| 항목 | 값 |
| --- | --- |
| msb | `~/.microsandbox/bin/msb`(또는 `MSB_PATH`) |
| Docker | 커널을 컴파일할 컨테이너(기본 `ubuntu:24.04`) |
| 네트워크 | libkrunfw 리비전 조회(GitHub API)와 커널 소스·패키지 다운로드 |
| 아키텍처 | aarch64·x86_64 모두. 호스트와 다른 쪽은 컨테이너 안에서 크로스 컴파일한다 |

## 빌드

```bash
task libkrunfw                                  # 이 머신 아키텍처 하나: target/libkrunfw/<arch>/
task libkrunfw:all                              # aarch64와 x86_64 둘 다
task libkrunfw:path                             # 이 머신이 로드할 산출물 경로(없으면 먼저 빌드)
sh scripts/build-libkrunfw.sh --help            # 같은 일을 옵션과 함께
sh scripts/build-libkrunfw.sh --list-fragments   # 어떤 조각이 있고 각각 무엇을 켜는지
sh scripts/build-libkrunfw.sh --arch all --revision 56bf194627a07996dc4ce21211fe86ada4c76080 --force
```

| 옵션 | 뜻 |
| --- | --- |
| `--fragments a,b` | 적용할 조각 이름(기본: `fragments/`의 모든 조각). 적은 순서와 상관없이 이름순으로 적용한다. `LIBKRUNFW_FRAGMENTS`도 같은 값이다 |
| `--list-fragments` | 조각 목록과 각 파일 맨 위에 적은 목적을 출력하고 끝낸다(빌드 없음) |
| `--revision SHA` | 빌드할 libkrunfw 리비전(기본: 설치된 msb 버전이 고정한 리비전) |
| `--msb VERSION` | 이 커널을 쓸 msb 버전(기본: 리비전을 정한 설치된 msb). `--revision`을 직접 줄 때만 필요하다 |
| `--arch aarch64\|x86_64\|all` | 만들 게스트 커널(기본: 이 머신). `all`은 둘 다, 이 머신 것을 먼저 |
| `--out DIR` | 출력 디렉터리(기본 `target/libkrunfw`), 산출물은 그 아래 `<arch>/`에 둔다 |
| `--force` | 리비전과 설정이 같아도 다시 빌드한다 |
| `LIBKRUNFW_IMAGE` | 빌드 컨테이너 이미지(기본 `ubuntu:24.04`) |

리비전은 `msb --version`의 버전으로 microsandbox의 `vendor/libkrunfw` gitlink를 GitHub API에서
읽어 정한다. 그래서 설치된 msb와 같은 커널을 고치게 되고, 네트워크가 없거나 답이 다르면
`--revision`으로 직접 지정한다. 리비전을 직접 주면 그 리비전이 설치된 msb가 고정한 것과 같은지
한 번 더 확인해 같을 때만 `msb`를 기록하고, 다르거나 확인할 수 없으면 `msb none`을 기록한다.
그런 커널은 farm이 호스트에 기본으로 설치하지 않으며 `--msb`로 대상을 선언하거나
`FARM_LIBKRUNFW_PATH`로만 쓸 수 있다.

빌드는 다음 순서로 진행된다.

1. `target/libkrunfw/libkrunfw`에 libkrunfw 저장소를 두고 해당 리비전으로 체크아웃한다.
2. 컨테이너 안에서 커널 소스를 풀고, libkrunfw의 패치를 적용하고, `config-libkrunfw_<arch>`에
   선택한 조각의 설정을 `scripts/config`로 적용한 뒤 `olddefconfig`를 실행한다. 조각은 이름순으로
   공통 파일(`<name>.config`)을 먼저, 그 아키텍처 파일(`<name>.<arch>.config`)을 그다음에
   적용한다. 지금 조각이 켜는 심볼은 다음과 같다. `vfio-mtty.config`는 `IOMMU_SUPPORT`·`VFIO`·
   `VFIO_MDEV`·`VFIO_IOMMU_TYPE1`·`PCI`·`VFIO_PCI`·`SAMPLES`·`SAMPLE_VFIO_MDEV_MTTY`를 켜고,
   `neutron-netfilter.config`는 `NF_CONNTRACK_MARK`·`NETFILTER_XT_CONNMARK`·
   `NETFILTER_XT_TARGET_CONNMARK`·`NETFILTER_XT_MATCH_CONNMARK`를 켠다. `olddefconfig`가
   `VFIO_GROUP`·`VFIO_CONTAINER`·`VFIO_PCI_CORE`·`IOMMU_API`·`IOMMU_DMA` 같은 의존 심볼을
   채운다. 게스트 커널이 arm64면 `ARCH=arm64`, x86_64면 `ARCH=x86_64`를 주고, 호스트와 다르면
   `CROSS_COMPILE=aarch64-linux-gnu-`나 `CROSS_COMPILE=x86_64-linux-gnu-`를 함께 준다.
3. `make kernel.c`로 커널 이미지와 libkrunfw용 C 번들을 만든다.
4. 같은 컨테이너 안에서 번들을 링크해 대상 아키텍처의 Linux 라이브러리를 만든다. 크로스 빌드와
   macOS 호스트는 Linux `cc`를 호스트에서 쓸 수 없으므로 컨테이너의 (크로스) `gcc`가 링크한다.
   macOS 호스트에서는 이 머신 아키텍처의 dylib만 추가로 링크한다(아래 산출물 표).
5. 최종 `.config`가 **적용한 조각의 모든 줄**을 그대로 담고 있는지, 번들이 같은 커널 버전인지
   확인하고, 하나라도 어긋나면 실패로 끝낸다. `olddefconfig`가 줄을 바꿨다면 어떤 조각의 어떤
   줄이 무엇으로 바뀌었는지 함께 보여준다.

컨테이너는 `docker --platform`으로 에뮬레이션하지 않고 호스트 아키텍처로 그대로 실행한다.
커널 쪽만 배포판의 크로스 툴체인(`gcc-aarch64-linux-gnu`, `gcc-x86-64-linux-gnu`)으로 컴파일하므로
컴파일러는 네이티브 속도로 돈다. `ARCH`는 kbuild와 libkrunfw Makefile이 함께 읽어서, 컨테이너
환경변수로는 `GUEST_ARCH`·`KBUILD_ARCH`를 쓰고 `make`에만 `ARCH=`를 준다.

커널 소스는 호스트 볼륨에 풀지 않고 컨테이너 볼륨(`farm-libkrunfw-<arch>`) 안에서만 다룬다.
리눅스 커널에는 대소문자만 다른 파일(`xt_CONNMARK.c`와 `xt_connmark.c`)이 있어서, 기본 macOS
볼륨에 풀면 트리가 깨진다. 같은 볼륨에 커널 트리·타르볼이 남으므로 재빌드는 그대로 이어서 한다.

같은 리비전·기본 설정·조각(이름과 내용)·툴체인·빌드 이미지면 재실행이 산출물과 `.config` 검증만
하고 끝난다. 조각이 바뀌면 그때만 커널을 다시 컴파일한다. 캐시 키에는 컨테이너 이미지의 태그와
로컬 image id가 함께 들어가고, 아직 받지 않은 이미지는 태그로만 비교하므로 다음 실행이 한 번
다시 빌드할 수 있다. 체크아웃이 이미 그 리비전이면 `git fetch`도 건너뛴다. 리눅스 커널 빌드는
arm64 기준 2분 안쪽이고, 첫 실행은 컨테이너 패키지 설치 때문에 조금 더 걸린다.

## 산출물

산출물은 `<out>/<arch>/`에 아키텍처별로 둔다. 이전 버전은 `<out>/libkrunfw.5.dylib` 하나만
만들었으므로, 그 경로를 쓰던 명령은 아래 표의 경로로 바꿔야 한다.

| 경로 | 언제 | 쓰는 곳 |
| --- | --- | --- |
| `<out>/aarch64/libkrunfw.5.dylib` | macOS 호스트가 arm64일 때 | macOS arm64의 msb |
| `<out>/x86_64/libkrunfw.5.dylib` | macOS 호스트가 x86_64일 때 | macOS x86_64의 msb |
| `<out>/<arch>/libkrunfw.so.<버전>` | 항상(두 아키텍처 모두) | Linux 호스트의 msb |
| `<out>/<arch>/manifest.txt` | 항상 | 이 커널이 무엇으로 만들어졌는지 확인할 때(farm 포함) |

`manifest.txt`는 한 줄에 `키 값` 꼴로 적는다. 리비전, 이 커널을 쓸 msb 버전(`msb`), 커널 버전,
아키텍처, kbuild 아키텍처, 툴체인, 적용한 조각 파일과 각 파일의 sha256, 그리고 그 아키텍처
산출물의 sha256이 들어간다. farm은 `msb`가 호스트의 msb와 같을 때만 이 커널을 부팅에 쓴다.
같은 조각·리비전이면 다시 빌드해도 같은 해시가 나오므로, 산출물과 manifest를 비교해 지금
로드하는 커널이 어느 빌드인지 확인할 수 있다.

dylib은 링크한 머신의 아키텍처로만 만들고, 다른 아키텍처의 dylib은 만들지 않는다. macOS가 그
아키텍처의 dylib을 로드할 수 없기 때문이다. Linux 라이브러리는 어느 호스트에서 빌드했든 대상
아키텍처의 ELF라서, 그 아키텍처의 Linux 호스트에 복사해 쓰면 된다.

## 사용

### farm의 기본 커널

`task dev`는 커널을 빌드한 뒤 데몬과 포탈을 띄우고, `farm host prepare`(로컬은 `farm host local`)는
호스트의 msb를 맞춘 다음 그 호스트 아키텍처의 커널을 설치한다. 설치 위치는
`~/.local/share/farm/libkrunfw/<id>/`이고 `id`는 `manifest.txt`의 sha256 앞 16자다. 새 커널을
설치하면 그 호스트의 이전 설치는 지운다. `~/.microsandbox`는 절대 쓰지 않는다.

```bash
task dev                                  # 커널 빌드(실패해도 계속) + 데몬 + 포탈
farm host local                           # 이 머신을 준비: msb, farm 커널, 검증 VM
farm host list                            # · custom kernel …/libkrunfw/<id>/… · kernel 6.12.113 · VFIO · mdev · mtty-1×24
```

coordinator(로컬 호스트를 실행하는 프로세스)와 각 호스트의 farm-agent는 시작할 때 이 설치를
찾아 microsandbox SDK에 `msb`와 라이브러리 짝을 넣는다. 설치된 커널의 `msb`가 그 호스트의 msb와
다르면 쓰지 않고 stock 커널로 부팅하며, 이유를 `farm host list`와 호스트 정보의
`custom_kernel_reason`에 남긴다. 호스트 준비가 msb를 먼저 맞추므로 보통은 그다음 요청부터
커널이 쓰인다. SSH 호스트도 준비 단계에서 같은 경로로 Linux `.so`를 업로드하므로 mtty farm을
그 호스트에 만들 수 있다.

`FARM_LIBKRUNFW_PATH`는 최우선 override다. 저장소 밖에서 만든 커널을 그 프로세스에서만 시험할
때 쓴다. `FARM_LIBKRUNFW_DIR`은 빌드 산출물 디렉터리를 바꾸며, 기본값은 실행 파일 위의
`libkrunfw`(예: `target/release/farm`이면 `target/libkrunfw`)다. `MSB_PATH`는 그대로 msb 실행
파일을 정한다.

### msb CLI로 직접 시험

`msb` CLI로 직접 시험할 때는 `MSB_LIBKRUNFW_PATH`와 `MSB_PATH`를 함께 준다. microsandbox SDK가
`MSB_LIBKRUNFW_PATH`만 있는 환경을 거절하기 때문이다(설치된 커널과 짝이 맞지 않은 채로 도는 것을
막으려는 규칙). farm은 그 짝을 스스로 맞춘다.

```bash
export MSB_PATH="$HOME/.microsandbox/bin/msb"
export MSB_LIBKRUNFW_PATH="$PWD/target/libkrunfw/aarch64/libkrunfw.5.dylib"   # macOS
# export MSB_LIBKRUNFW_PATH="$PWD/target/libkrunfw/x86_64/libkrunfw.so.5.6.1"  # Linux

msb run --no-tty alpine:3.20 -- sh -c 'ls /sys/class/mdev_bus'
msb run --no-tty alpine:3.20 -- sh -c '
  cat /sys/devices/virtual/mtty/mtty/mdev_supported_types/mtty-1/{name,device_api,available_instances}
  echo "$(cat /proc/sys/kernel/random/uuid)" > /sys/devices/virtual/mtty/mtty/mdev_supported_types/mtty-1/create
  ls /sys/bus/mdev/devices/ /dev/vfio'
# Neutron's L3 agent writes these two into every router's mangle table; both need CONNMARK.
msb run --no-tty alpine:3.20 -- sh -c '
  apk add -q iptables
  iptables -t mangle -A PREROUTING -j CONNMARK --restore-mark --nfmask 0xffff0000 --ctmask 0xffff0000
  iptables -t mangle -A PREROUTING -j CONNMARK --save-mark --nfmask 0xffff0000 --ctmask 0xffff0000
  ip6tables -t mangle -A PREROUTING -j CONNMARK --restore-mark --nfmask 0xffff0000 --ctmask 0xffff0000
  ip6tables -t mangle -S PREROUTING'
```

다른 farm 명령(`plan`, `catalog`, `openapi`)은 msb가 필요 없어서 override 경로가 틀려도 동작하고,
VM을 만드는 명령만 파일이 아니면 실패한다.

기대하는 결과는 다음과 같다.

| 확인 | 값 |
| --- | --- |
| `dmesg` | `VFIO - User Level meta-driver version: 0.3`, `mtty mtty: MDEV: Registered` |
| `/sys/class/mdev_bus/mtty` | 존재 |
| `mtty-1` 속성 | `Single port serial`, `vfio-pci`, `available_instances 24` |
| `mtty-1/create`에 UUID 쓰기 | 성공하고 `available_instances`가 줄어든다 |
| `/dev/vfio/<그룹>` | 생기고, 그룹 번호가 `/sys/kernel/iommu_groups`에 나타난다 |
| `iptables`/`ip6tables -t mangle -j CONNMARK` | 성공하고 `-S`에 규칙이 보인다(`/proc/modules` 없음) |

## 조각 파일 형식

조각은 `scripts/libkrunfw/fragments/<name>.config`이고, 한 아키텍처에만 필요한 설정은
`<name>.<arch>.config`에 둔다(`arch`는 `aarch64` 또는 `x86_64`). 둘 다 있으면 공통 파일을 먼저,
아키텍처 파일을 그다음에 적용한다. 파일 맨 위의 `#` 주석이 그 조각의 목적이고,
`--list-fragments`가 그 주석을 그대로 보여준다.

파일은 커널 `.config` 문법을 그대로 쓴다. 빈 줄과 `#`로 시작하는 주석 줄을 허용하고, 나머지는
다음 네 형태만 받는다.

| 줄 | 뜻 | 적용 |
| --- | --- | --- |
| `CONFIG_FOO=y` | 내장 | `scripts/config --enable` |
| `CONFIG_FOO="문자열"` | 문자열 값 | `--set-str` |
| `CONFIG_FOO=123` | 숫자 값(16진수 `0x...` 포함) | `--set-val` |
| `# CONFIG_FOO is not set` | 끄기 | `--disable` |

`=m`은 거절한다. 게스트 커널에 모듈 로딩이 없어서 모듈을 만들 수 없기 때문이다. `=n`도 받지
않고 위의 "is not set" 형태를 쓰라고 알려 준다.

기본처럼 모든 조각을 적용할 때 `name.<arch>.config`만 있는 조각은 다른 아키텍처에서 빠진다.
`--fragments`로 이름을 직접 주면 그 아키텍처에 파일이 없을 때 실패한다. 이름을 잘못 적은 것을
조용히 넘기지 않으려는 것이다.

## 새 커널 옵션 추가하기

1. `scripts/libkrunfw/fragments/`에 조각 파일을 하나 만든다. 이름은 기능 이름으로 짓고, 맨 위에
   한두 줄로 목적을 적는다. `--list-fragments`에 그 주석이 그대로 나온다. 두 아키텍처에 같은
   설정이면 `<name>.config`, 한쪽에만 필요하면 `<name>.<arch>.config`를 쓴다. 예를 들어 네트워크
   관련 옵션이면 `net-tune.config`를 만들고 필요한 `CONFIG_...=y` 줄을 적는다.
2. `task libkrunfw`(또는 `task libkrunfw:all`)로 빌드한다. 기본이 모든 조각이므로 방금 만든 파일이
   바로 적용된다. 일부만 시험하려면 `task libkrunfw -- --fragments vfio-mtty,net-tune`처럼 이름을
   준다.
3. 빌드가 끝나면 `settings N from <조각들>` 줄에서 검증한 설정 수와 조각 이름을 확인하고,
   `<out>/<arch>/manifest.txt`에서 조각별 sha256과 산출물 sha256을 확인한다.

`olddefconfig`가 조각의 줄을 바꾸면(의존 심볼이 빠졌거나 그 아키텍처에 심볼이 없을 때) 빌드가
다음처럼 실패한다.

```
<out>/build-<arch>/kernel.config does not keep what the fragments ask:
  scripts/libkrunfw/fragments/net-tune.config asks CONFIG_XYZ=y; <out>/build-<arch>/kernel.config has # CONFIG_XYZ is not set
```

이 메시지가 나오면 그 줄이 필요로 하는 설정을 같은 조각(또는 그 아키텍처 전용 조각)에 함께 넣고
다시 빌드한다. 한 조각이 두 아키텍처에서 다르게 성립하면 공통 파일 대신 `<name>.<arch>.config`로
나눈다.

두 조각이 같은 심볼에 다른 값을 적으면 컨테이너를 띄우기 전에 어느 두 줄이 부딪히는지 보여 주고
멈춘다. 같은 심볼에 같은 값을 두 번 적는 것은 문제가 아니다. 이 규칙 때문에 한 조각이 다른 조각의
값을 덮어쓸 수는 없고, 값을 바꾸려면 원래 조각을 고친다.

## 아키텍처별 설정 차이

`vfio-mtty.config`와 `neutron-netfilter.config`는 두 아키텍처에 같은 심볼을 켠다. 나눠 둘 필요가
없었던 이유는, 두 기본 설정 모두에서 `olddefconfig`가 의존 심볼을 채워 대상 심볼이 전부 `=y`가
되기 때문이다(`NETFILTER_ADVANCED`는 두 기본 설정에서 이미 `y`라 CONNMARK가 성립한다).

- aarch64 기본 설정은 `IOMMU_SUPPORT`·`PCI`·`VFIO`·`SAMPLES`가 꺼져 있어 조각 파일이 그대로 켠다.
- x86_64 기본 설정은 포크에서 `CONFIG_PCI`와 `CONFIG_ACPI`가 꺼져 있다. 그래도 `CONFIG_PCI=y`와
  `CONFIG_VFIO_PCI=y`가 성립하고(`PCI_DIRECT`, `IRQ_MSI_IOMMU`가 따라 켜진다), `ACPI` 없이
  VFIO·mdev·mtty가 빌드·부팅된다. 그래서 조각 파일에 x86_64 전용 심볼을 더하지 않았다.
  `CONFIG_ACPI`는 끈 채로 둔다: libkrunfw의 x86_64 기본 설정이 빠른 부팅을 위해 뺀 값이고,
  VFIO/mdev 경로가 ACPI에 의존하지 않는다.
- 다만 x86_64에서 `CONFIG_PCI_MSI`는 `olddefconfig` 기본값이 `n`이라 꺼진 채다(aarch64는 `y`).
  mtty는 PCI 장치가 아니라서 mdev 생성·VFIO 그룹 생성에 영향이 없고, 이 커널의 시험 범위도
  mtty까지다. 실제 GPU mdev를 이 커널로 넘기는 단계에서는 `CONFIG_PCI_MSI=y`를 조각 파일에
  추가해야 한다.

한쪽 아키텍처에서 대상 심볼이 꺼지면 조각 파일을 `vfio-mtty.<arch>.config`로 나누고 이 표를
고친다.

## 릴리스 배포 (TODO)

지금 커널은 **개발 산출물**(`target/libkrunfw`)에서만 설치된다. 릴리스 빌드에는 이 디렉터리가
없으므로 `farm host prepare`는 "farm 커널 없음" 이벤트를 남기고 stock 커널로 동작한다. agent
아티팩트처럼 릴리스 버킷에서 받아 오려면 다음이 필요하다.

- 릴리스 파이프라인이 `farm-agent` 매니페스트와 같은 버킷(`public-bucket.preinpost.in`)에
  `<kernel-id>/<arch>/manifest.txt`와 산출물을 게시한다. `<kernel-id>`는 이 문서의 manifest
  sha256 앞 16자다. macOS dylib은 coordinator와 같은 아키텍처에만 필요하고, SSH 호스트는 Linux
  `.so`만 받는다.
- farm은 `FARM_LIBKRUNFW_DIR`이 없거나 로컬 산출물이 없을 때 그 버킷을 조회해, 이 빌드의
  `msb`와 맞는 커널만 내려받아 설치 전에 sha256을 확인한다.
- 게시 경로와 공개 여부는 agent 매니페스트와 같게 두고, 서명·만료 정책도 함께 정한다.

이 절이 구현되기 전까지는 `target/libkrunfw`가 있는 개발 체크아웃에서만 mtty가 동작한다.

## 한계

- **기본 커널은 개발 산출물에서 설치된다.** 릴리스 바이너리에는 커널 산출물이 함께 실리지
  않으므로 위의 배포 훅이 먼저 필요하다. `FARM_LIBKRUNFW_PATH`는 그 전에도 쓸 수 있는 override다.
  호스트 준비 단계의 msb 설치는 `~/.microsandbox/{bin,lib}`만 쓰므로 farm이
  `~/.local/share/farm/libkrunfw`에 둔 커널은 덮이지 않는다. 다만 override가
  `~/.microsandbox/lib` 안을 가리키면 farm은 msb 재설치를 건너뛰고 손으로 설치하라고 안내한다.
- **dial-in agent 호스트는 coordinator가 커널을 밀어 넣을 수 없다.** `farm agent connect`로 붙는
  호스트는 그 머신에 설치된 커널이나 `FARM_LIBKRUNFW_PATH`를 스스로 쓴다. 설치되어 있지 않으면
  stock 커널로 부팅하고 mtty farm의 게이트에서 거절된다.
- 모듈이 없으므로 필요한 기능은 모두 내장(`=y`)이어야 한다. 커널 이미지가 그만큼 커진다
  (arm64 설치 커널 24,933,536 → 25,461,920 bytes, 약 2%).
- 커널은 자기가 컴파일된 아키텍처에서만 부팅한다. 게스트 아키텍처는 호스트 아키텍처와 같아야
  한다. 빌드는 어느 호스트에서나 두 아키텍처를 만들 수 있지만, macOS 호스트는 dylib으로만
  라이브러리를 로드하므로 macOS에서는 이 머신 아키텍처의 커널만 시험할 수 있다. x86_64 커널은
  x86_64 Linux에서, arm64 커널은 arm64 Linux나 arm64 macOS에서 부팅한다.
- `nova_libvirt` 컨테이너의 mdev 시야는 PoC에서 확인했다(`target/gpupoc-report/REPORT.md` 2.4).
  privileged·host `/dev` 바인드로 도는 컨테이너가 `/sys/class/mdev_bus/mtty`를 보고 `create`에
  쓸 수 있으며 `/dev/vfio`도 보인다. 다른 방식으로 배포한다면 그 컨테이너 마운트를 다시 확인한다.
- mdev 인스턴스는 재부팅하면 사라진다. farm의 재배포·스냅샷 복원 뒤에 Nova가 다시 만드는지는
  이 커널만으로 정해지지 않는다.

## 배포본 자동 다운로드

로컬에 빌드 산출물(`<dir>/<arch>/manifest.txt`)이 없는 호스트는 farm이 배포한 커널을 받는다.
대상 URL은 `FARM_LIBKRUNFW_URL`(기본 `https://public-bucket.preinpost.in/farm-libkrunfw`)이고,
빈 값이면 끈다. 준비 순서는 로컬 산출물 → 배포본이며, 받은 커널은 SHA-256·크기·매니페스트 해시
(`kernel::id_for`)·msb·arch를 모두 확인한 뒤 `<state-dir>/libkrunfw-cache/<kid>/`에 캐시하고
기존 설치 경로로 호스트에 올린다. 같은 kid가 캐시에 있으면 매니페스트와 라이브러리는 다시 받지 않는다.

```
farm-libkrunfw/<kid>/manifest.txt                  immutable
farm-libkrunfw/<kid>/<os>-<arch>/<library>         immutable (예: linux-x86_64/libkrunfw.so.5.6.1)
farm-libkrunfw/msb-<msb>/<os>-<arch>.json          max-age=60 (인덱스: kid, sha256, size, 경로)
```

`<kid>`는 `manifest.txt` 바이트의 SHA-256 앞 16자리다. 업로드는
`.forgejo/workflows/libkrunfw.yml`이 `linux-x86_64`(조각 3개)와 `linux-aarch64`(조각 2개)를 따로
빌드해 올린다(`scripts/libkrunfw-publish.sh kid|index`로 키와 인덱스를 만든다). `darwin-aarch64`는
같은 레이아웃을 쓰지만 macOS 링커가 필요해 Linux CI에서는 만들지 않는다.
