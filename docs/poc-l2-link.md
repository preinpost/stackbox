# PoC: WebSocket으로 VM 간 L2 잇기 (2026-11-02)

msb는 VM끼리 트래픽을 중계하지 않는다. 역할별 VM(control, compute)을 한 farm으로 묶으려면
VM 사이에 L2가 필요하다. 이 PoC는 VM 안의 TAP을 WebSocket으로 호스트의 스위치에 연결해
그 L2를 만들고, OpenStack 내부망으로 쓸 만한지 쟀다.

```
[farm-poc-a] farm0(TAP, 10.254.0.2) ⇄ farm-link link ─ws─┐
                                                          ├─ farm-link switch (호스트, MAC 학습)
[farm-poc-b] farm0(TAP, 10.254.0.3) ⇄ farm-link link ─ws─┘
```

- `src/bin/farm-link.rs`(`71594d0`, 지금은 `src/link/`): `switch`(호스트), `link`(게스트, TAP ⇄ WebSocket), `sink`/`send`(기준 측정).
  WebSocket 메시지 하나가 Ethernet 프레임 하나다. 일괄 전송 같은 최적화는 하지 않았다.
- 스위치는 포트마다 지연과 손실을 넣는다(`POST /impair?name=b&delay_ms=50&loss=0.01`).
  지연은 그 포트로 들어가고 나가는 양방향에 걸리므로 RTT는 두 배만큼 늘어난다.
- msb 네트워크 코드에는 VM에 두 번째 NIC를 소켓으로 붙이는 기능이 없다. 그래서 L2는 게스트 안의
  TAP으로 만든다. 게스트 커널(6.12.113)에 `/dev/net/tun`이 있다. 게스트는 호스트의 loopback에
  `host.microsandbox.internal`로 닿으므로 스위치는 외부 포트를 열지 않는다.
- 재현: 아래 "PoC 재현" 참고.

## 결과

Apple M5 Pro, msb 0.7.13, VM 2대(2 vCPU, 1 GiB).

| 항목 | 결과 |
|---|---|
| 기준: 게스트 → 호스트 TCP (msb NAT만) | 7.5 Gbit/s, RTT p50 0.16 ms |
| VM ↔ VM ping (MTU 1500) | p50 1.1 ms, p99 1.7 ms |
| VM ↔ VM iperf3 (MTU 1500) | 1.15 Gbit/s 양방향 (Nagle을 켜면 2.0 Gbit/s지만 RTT가 늘어난다) |
| VM ↔ VM iperf3 (MTU 9000) | 4.1 Gbit/s 양방향, 재전송 0, 8972 B DF ping 통과 |
| 지연 50 ms 주입 | RTT 106 ms, iperf3 150 Mbit/s |
| 손실 5% 주입 (양방향) | ping 손실 6.7%, iperf3 9.7 Mbit/s |
| 분리(손실 100%) → 복구 | ping 0% → 즉시 복구 |
| 스위치 재시작 | 두 링크가 수 초 안에 다시 붙고 통신 재개 |

손실은 스위치가 프레임을 버리는 것이라 바깥 TCP(WebSocket)는 재전송하지 않는다. 안쪽 TCP가
실제 손실 링크처럼 반응하므로 장애 재현이 정확하다.

## 발견한 것

- 지연이 0인 프레임도 tokio 타이머를 거치면 1 ms 단위로 기다린다. 대기열이 네 번이라 RTT가
  6 ms였다. 지연이 없으면 타이머를 건너뛰어 1.1 ms가 됐다.
- `TCP_NODELAY`는 지연에는 영향이 작고 처리량을 2.0 → 1.15 Gbit/s로 줄였다. 지연 원인은 위의 타이머였다.
  본 구현에서는 nodelay를 유지하고 프레임을 묶어 보내 처리량을 되찾는다.
- MTU를 9000으로 올리면 메시지 수가 줄어 처리량이 네 배 가까이 된다. farm 내부망은 가상이라
  jumbo frame을 기본으로 써도 된다. 대륙 간 UDP 전송에서는 다시 낮춰야 한다.

## 판단

OpenStack 내부망(RabbitMQ, DB, API, Neutron VXLAN 터널, 이미지 전송)으로 충분하다.

# 진행 상황 (2026-11-02 기준)

## 목표

테스트용 OpenStack 클러스터에서 host evacuate 같은 장애 대응을 시험한다. 지금 farm은 VM 1대에
all-in-one으로 배포하므로 compute가 하나뿐이라 evacuate를 할 수 없다.

## 정한 설계

- **역할별 VM**: farm을 control VM과 compute VM 여러 대로 나눈다. 첫 구성은 control 1 + compute 2.
  control 2대는 Galera·RabbitMQ 과반수가 안 돼 HA가 아니므로, HA가 필요하면 3대로 간다.
- **L2는 farm-link**: VM 안의 TAP(`farm0`)을 WebSocket으로 스위치에 잇는다. `farm0`는 지금의
  고정 서비스 주소용 dummy를 대신하고, VM마다 `10.254.0.x`를 받는다. Kolla 내부망과 Neutron
  터널망이 이 위에 올라간다. MTU는 9000을 기본으로 한다.
- **스위치는 각 호스트의 farm-agent 안에** 둔다. 같은 호스트의 VM끼리는 coordinator를 거치지 않는다.
  coordinator는 토폴로지, 인증, 장애 주입 같은 제어만 맡는다. coordinator가 재시작해도 farm의
  네트워크가 끊기면 안 된다(`bc0704e`의 방향과 같다).
- **호스트 간 연결**: 처음에는 기존 agent WebSocket으로 coordinator가 중계한다. 다음에 agent끼리
  UDP 직접 연결(QUIC datagram이나 WireGuard 방식)과 NAT hole punching을 붙이고, 실패하면
  중계로 돌아간다. 대륙 간에는 TCP 위 TCP를 피해 UDP로 보내고, MTU를 1380 정도로 낮추며
  `global_physnet_mtu`도 맞춘다.
- **배치 규칙**: control은 한 지역에 모은다. Galera·RabbitMQ 클러스터는 대륙 간 지연을 못 견딘다.
  compute만 멀리 둘 수 있다(OpenStack Edge/DCN 패턴).
- **장애 주입**: 스위치의 링크별 지연·손실·분리를 포탈과 API에서 다룬다. 대륙 간 환경도 로컬에서
  지연과 손실로 흉내 낸다.
- **공유 스토리지는 생성 시 선택 항목**으로 둔다(Horizon, Neutron처럼). 선택지:
  1. 공유 인스턴스 스토리지: compute들의 `/var/lib/nova/instances`를 같이 쓴다. evacuate 뒤에도
     디스크가 남고 live migration도 된다. VM이 나뉘므로 control VM의 NFS 서버를 farm L2로 마운트하는
     방식이 된다(게스트 커널의 NFS 클라이언트 지원 확인 필요).
  2. Cinder(LVM): 볼륨 기반 evacuate. msb 커널의 iSCSI 지원 확인 필요.
  3. Ceph: 운영과 가장 비슷하지만 무겁고 cephadm이 따로 필요하다. 나중에.
- **스냅샷**: 여러 VM을 한 세트로 멈추고 얼리고, 복원할 때 MAC·IP·hostname을 같이 맞춰야 한다.
  지금의 VM 1대 얼리기를 확장한다.

검토하고 미룬 대안: VM 하나 안에 privileged 시스템 컨테이너로 노드를 나누는 방식. 같은 커널이라
네트워크와 스냅샷이 쉽지만, 장애가 커널 단위로 재현되지 않고 호스트를 넘을 수 없다.

## 다음 단계

1. ~~control 1 + compute 2 multinode 배포와 evacuate~~ 완료. 아래 "multinode와 evacuate" 참고.
2. ~~스위치를 제품 안으로 옮기고 링크 인증, 프레임 묶음 전송~~ 완료. 아래 "제품의 링크" 참고.
3. ~~farm 모델에 노드 구성(control 1 + compute 1~3)을 넣고 plan, 배포, 장애 주입 API, 포털을 바꾼다~~ 완료.
   아래 "제품의 다중 VM farm" 참고.
4. 다음 후보 세 가지(공유 스토리지, 여러 VM 스냅샷, 호스트 간 중계)와 남은 검증은 아래 "다음 후보"에 정리했다.
   순서는 아직 정하지 않았다.

## multinode와 evacuate (2026-11-02, Apple M1 Max 32 GiB)

PoC 스크립트(`scripts/poc-multinode.sh`, `scripts/poc-multinode/`)는 `71594d0`에 있다. 제품의 링크로
대체해서 지금 브랜치에서는 지웠다. 재현은 아래 "PoC 재현" 참고.

- **구성**: `mn-ctl`(4 vCPU, 6 GiB), `mn-c1`·`mn-c2`(2 vCPU, 3 GiB). VM마다 링크가 두 개다.
  `farm0`(`10.254.0.10~12`, MTU 9000)은 Kolla의 network·api 인터페이스이고, `farm1`(주소 없음)은
  OVS `br-tenant`의 physnet1이다. 링크는 `farm-link@.service`로 부팅 때마다 다시 붙는다.
- **배포**: Keystone, Glance, Placement, Nova, Neutron(OVS netdev). kolla-ansible은 `mn-ctl`에서
  compute로 SSH 배포한다. 처음부터 끝까지 약 35분이 걸렸고, 끝난 뒤 nova-compute 2개,
  OVS agent 3개, DHCP·L3·Metadata agent가 모두 up이었다. 배포 뒤 메모리 사용량은 ctl 3.3 GiB,
  compute 각 0.8 GiB다.
- **테넌트망은 VXLAN 대신 VLAN**(physnet1:100:199)이다. userspace datapath에서 VXLAN을 쓰려면
  터널 주소가 OVS 브리지에 있어야 해서 PoC에서는 피했다. 외부망 `public`은 ctl의 `farm-ext`
  (physnet2, flat)이고 라우터와 DHCP는 ctl에 있다.
- **evacuate 결과**

  | 단계 | 결과 |
  |---|---|
  | Cirros를 `mn-c1`에 부팅 (VLAN 164) | ACTIVE, ctl의 qrouter에서 ping 통과 |
  | `c1`, `c1-t` 포트 손실 100% | nova가 55초 뒤 `mn-c1`을 down으로 표시 |
  | `compute service set --down` → `server evacuate --host mn-c2` | 18초 뒤 `mn-c2`에서 ACTIVE, 같은 IP로 ping 통과 |
  | 연결 복구, `--up`, c1의 `nova_compute` 재시작 | c1에 남아 있던 원래 인스턴스를 nova가 지움, 두 compute 모두 up |

  고립된 동안 c1의 원래 인스턴스는 계속 실행 중이었다. 스위치 분리가 fencing 역할을 해서 같은
  IP가 충돌하지 않았다. 디스크는 공유하지 않으므로 evacuate는 이미지로 다시 만든다.
- M1 Max는 nested virtualization이 없어 게스트에 `/dev/kvm`이 없고 Nova는 qemu TCG로 돈다.
- 같은 머신의 L2 측정: NAT 기준 4.7 Gbit/s(RTT 평균 0.33 ms), MTU 1500 2.0 Gbit/s(ping 평균 2.0 ms),
  MTU 9000 2.9 Gbit/s(재전송 0, 8972 B DF ping 통과).

본 구현에 넣을 것:

- kolla의 `kolla_container`는 docker와 dbus를 import한다. compute가 ctl로 delegate하는 nova-cell
  task(hypervisor·cell 목록)는 compute의 interpreter인 `/usr/bin/python3`로 ctl에서 돈다. bootstrap은
  ctl의 venv에만 docker SDK를 넣으므로 ctl의 시스템 python에 `python3-dbus`와 `docker`를 따로 설치한다.
  `python3-dbus`는 이제 golden image(`guest/os/rocky10.sh`)에 들어 있다.
- 예전 Docker 이미지의 `authorized_keys`는 끝에 개행이 없었다. 지금 golden image에는 이 파일이 없다.
- `msb copy`는 실행 권한을 남기지 않는다. 복사 뒤 `chmod`한다.
- 게스트 hostname은 VM마다 달라야 한다(`mn-ctl` 등). 지금의 고정 `farm-aio`는 multinode에서 쓸 수 없다.

## 제품의 링크 (2026-11-03, Apple M1 Max)

PoC의 `farm-link`를 `src/link/`로 옮겼다.

- **스위치는 호스트마다 프로세스 하나**다. agent가 필요할 때 `agent switch <agent dir>/switch`로 띄운다
  (`Switch::ensure`). deploy worker처럼 setsid로 떨어져 나가므로 agent·coordinator가 재시작해도 farm 네트워크는
  끊기지 않는다. `switch.lock`으로 하나만 돌고, 처음 고른 loopback 포트를 `addr`에 남겨 재시작해도 같은 포트로
  연다. 게스트의 설정에 그 포트가 들어가기 때문이다.
- **포트 설정은 파일**(`ports.json`)이다. agent가 `ports.lock` 아래에서 통째로 바꾸고, 스위치는 0.5초마다 보고
  따라간다. 포트마다 segment(`<farm>/<망>`), 토큰의 SHA-256, 지연·손실이 있다. 포트가 빠지거나 토큰이 바뀌면
  그 연결을 끊는다. segment가 다르면 프레임이 건너가지 않으므로 farm·망마다 따로 된 L2다.
- **연결은 WebSocket**이다. 게스트가 `GET /link?port=<port>`에 `Authorization: Bearer <token>`을 붙여 열고,
  모르는 포트나 틀린 토큰은 401로 거절된다. 지금은 호스트 loopback만 지나지만, 나중에 링크가 호스트 밖으로
  나갈 때 HTTP 프록시·TLS(`wss://`)·로드밸런서를 그대로 지나도록 WebSocket을 쓴다. 게스트 설정의 `SWITCH`도
  URL(`ws://host.microsandbox.internal:<port>/link`)이다.
- **메시지 하나에 프레임 여러 개**를 담는다. 프레임마다 2바이트 길이를 앞에 붙이고, 양쪽 모두 쌓인 프레임을
  한 메시지로 보낸다(최대 256 KiB). 그래서 MTU 1500에서도 처리량이 MTU 9000과 비슷하다. 같은 묶음 전송을 한
  TCP 길이 접두 방식과 견줘 처리량 차이는 측정 오차 안이었다.
- **게스트**는 Linux farm-agent 바이너리를 `/usr/local/sbin/farm-agent`로 받고, `farm-link@<dev>.service`가
  `agent link --setup`으로 영구(persistent) TAP과 MTU·주소를 만든 뒤 `agent link`로 붙는다. 링크 프로세스가
  재시작해도 장치와 주소가 남아 그 주소에 바인딩한 서비스가 살아 있다. 바이너리는 Linux 호스트에서는 agent
  자신이고, 그 밖에서는 `FARM_GUEST_AGENT`로 준다(macOS 개발 머신은 `dist/agent/aarch64/farm-agent`).
- `create_node`가 `LinkConf` 목록을 받으면 VM을 만든 뒤 링크를 설치한다. 아직 이것을 쓰는 farm 구성은 없다(3번).

실제 VM 3대로 확인했다(`tests/farm_link.rs`, 아래 명령).

| 항목 | 결과 |
|---|---|
| 같은 segment a ⇄ b / 다른 segment c | 통신 / 닿지 않음 |
| iperf3 MTU 9000 | 2.7 Gbit/s |
| iperf3 MTU 1500 | 2.8 Gbit/s (PoC 1.15, Nagle을 켜면 2.0) |
| 손실 100% → 복구, 지연 25 ms | 끊김 → 복구, RTT 55 ms |
| 링크 프로세스 재시작 | 주소 유지, 0.2초 뒤 통신 |
| 스위치 프로세스 SIGKILL → 다시 시작 | 같은 포트, 2초 뒤 통신 |
| 토큰 교체 | 옛 토큰의 게스트는 거절됨 |
| VM 재시작 | 부팅 때 링크가 다시 붙음 |

```bash
sh scripts/build-agent.sh aarch64     # 게스트용 Linux farm-agent
FARM_GUEST_AGENT=$PWD/dist/agent/aarch64/farm-agent \
  cargo test --release --test farm_link -- --ignored --nocapture
```

## 제품의 다중 VM farm (2026-11-03, Apple M1 Max 32 GiB)

`farm create mn --host-id local --computes 2`, `farm deploy mn`으로 만들었다(README의 "다중 VM farm").

- VM 3대가 5초 만에 만들어지고 링크 6개가 스위치에 붙었다. 배포는 prepare부터 verify까지 약 15분이었고,
  nova-compute 2개, OVS agent 3개, DHCP·L3·Metadata agent가 모두 up이었으며 verify의 Cirros ping이 통과했다.
  배포 뒤 메모리 사용량은 control 4.1 GiB, compute 각 0.8~0.9 GiB.
- `farm fault mn compute-1 --loss 1`로 고립시키자 nova가 60초 뒤 down으로 표시했다. `--down` 후
  `server evacuate --host farm-compute-2`로 7초 만에 ACTIVE가 되고 같은 IP로 ping이 통과했다.
  `farm fault mn compute-1`로 복구하고 `--up`·nova_compute 재시작 뒤 compute-1에 남은 원래 인스턴스가 지워졌다.
- daemon API로 compute-2에 지연 25 ms를 주자 compute 사이 RTT가 55 ms 이상으로 늘었다.
- 포털: 생성 화면의 **VM 구성**, 상세 화면 topology의 노드별 **고립·손실 5%·지연 50 ms·복구** 버튼.

## 다음 후보 (2026-11-03 정리)

### 1. 공유 스토리지 선택

**목표**: evacuate 뒤에도 인스턴스 디스크가 남게 하고, live migration도 시험할 수 있게 한다. 지금은 디스크를
공유하지 않아 evacuate가 이미지로 인스턴스를 새로 만든다.

**게스트 커널에서 확인한 것** (`mn-compute-1`, 6.12.113, modprobe 없음, `/proc/config.gz` 없음):

| 기능 | 결과 | 확인 방법 |
|---|---|---|
| NFS 클라이언트 | 없음 | `/proc/filesystems`에 `nfs` 없음 |
| CephFS·RBD 커널 드라이버 | 없음 | `/proc/filesystems`, `/sys/module` |
| iSCSI initiator | 없음 | `scsi_transport_iscsi`는 있으나 `/sys/class/iscsi_transport`가 비어 있음(`iscsi_tcp` 없음) |
| FUSE | 있음 | `/dev/fuse`, `/proc/filesystems`의 `fuse`·`fuseblk` |
| loop 장치 | 있음 | `/dev/loop-control` |

그래서 처음 적었던 선택지 중 NFS 공유 인스턴스 스토리지와 Cinder LVM(iSCSI)은 지금 커널로는 쓸 수 없다.
남는 선택지:

- **A. Ceph RBD (권장)**: Nova(`images_type = rbd`), Glance, Cinder가 커널 드라이버 없이 QEMU의 librbd로 디스크에
  붙는다. 운영 환경과 가장 비슷하고, evacuate는 디스크를 그대로 쓰며 live migration도 된다.
  - Kolla는 Ceph를 직접 배포하지 않으므로 cephadm으로 따로 띄운다. 처음에는 control VM(또는 storage VM 하나)에
    MON·MGR·OSD 하나씩, pool `size = 1`로 둔다.
  - OSD 장치는 loop 장치나 msb가 붙이는 디스크를 쓴다. msb의 owned disk는 ext4로 포맷돼 나오므로 raw 블록이
    필요하면 그 위 파일을 loop로 쓰는 방식을 먼저 시험한다.
  - 메모리는 MON·MGR·OSD 합쳐 2~3 GiB 정도를 예상한다(측정 전 추정).
- **B. FUSE 공유 인스턴스 스토리지**: compute들의 `/var/lib/nova/instances`를 FUSE 파일시스템(GlusterFS 클라이언트,
  ceph-fuse, sshfs 중 하나)으로 함께 마운트한다. 가볍지만 QEMU 파일 잠금과 성능을 따로 확인해야 하고, 운영
  환경과는 거리가 있다. A가 막힐 때의 대안으로 둔다.
- **C. msb 커널에 NFS·iSCSI를 넣기**: libkrunfw 커널을 다시 빌드해야 하므로 범위 밖으로 둔다.

**제품에 넣을 것**
- Ceph는 farm마다 하나를 control VM에 띄우고 그 farm의 compute들이 RBD로 함께 쓴다. compute에는 keyring과
  `ceph.conf`만 둔다. 여러 farm이 한 Ceph를 나눠 쓰면 삭제·스냅샷·장애가 farm 밖에 묶이므로 피한다.
  `osd_memory_target`을 1 GiB 정도로 낮추는 것은 아직 시험하지 않았다.
- plan과 생성 요청에 `storage`(`none` | `rbd` | 나중에 `fuse`)를 넣는다. multinode farm에서만 고를 수 있게 한다.
- 메모리 예산과 디스크 예산에 스토리지 몫을 더한다. 포털 생성 화면에 선택 항목을 둔다.
- 배포에 cephadm 단계를 넣고, globals.yml에 외부 Ceph 설정(`nova_backend_ceph`, `glance_backend_ceph`)을 쓴다.
- verify에서 RBD로 부팅한 인스턴스를 evacuate 해 디스크 내용이 남는지(파일 하나를 써 두고 확인) 본다.

**먼저 확인할 것**: control VM 하나에서 cephadm 단일 노드 + loop OSD가 뜨는지, Nova가 그 pool로 Cirros를
부팅하는지, evacuate 뒤 디스크가 유지되는지.

**확인 결과 (2026-11-03, Apple M5 Pro 24 GiB, farm `ceph` = control 1 + compute 2)**: 세 가지 모두 된다. 제품 배포를
끝낸 뒤 control VM에서 손으로 했다.

| 단계 | 결과 |
|---|---|
| `cephadm --docker bootstrap --mon-ip 10.254.0.10 --single-host-defaults --skip-dashboard --skip-monitoring-stack` (tentacle 20.2.5, `rpm-tentacle/el10`) | MON·MGR up |
| `/var/lib/ceph-osd-img/osd0.img`(20 GiB) → `losetup` → OSD spec `method: raw` | OSD 1개 up |
| pool `vms`·`images`, `size 1` (`mon_allow_pool_size_one`) | `HEALTH_WARN`(OSD 1개)뿐 |
| `globals.yml`에 외부 Ceph 설정, `kolla-ansible reconfigure -t glance,nova,nova-cell` | compute의 `images_type = rbd`, Glance `default_backend = rbd` |
| Cirros를 compute-1에 부팅 | 30초 만에 ACTIVE, 디스크는 `vms/<id>_disk` |
| compute-1 `farm fault --loss 1` → `--down` → `server evacuate --host farm-compute-2` | 40초 안에 down, evacuate 20초 뒤 ACTIVE |
| 디스크 유지 확인 | 첫 부팅 때 만든 SSH host key가 evacuate 뒤에도 같다(이미지로 다시 만들면 바뀐다) |
| 링크 복구, `--up`, c1 `nova_compute` 재시작 | c1의 원래 인스턴스를 nova가 지움, 두 compute 모두 up |

게스트 환경 때문에 우회한 것(제품에 넣을 것):
- **udev 데이터가 없다**: ceph-volume이 `No udev data could be retrieved for /sys/block/vda`로 멈춘다.
  `systemd-udev`를 설치하고 `systemctl start systemd-udevd; udevadm trigger --subsystem-match=block`.
- **device-mapper가 없다**: 기본 `lvm batch`는 못 쓴다. OSD spec에 `method: raw`를 쓴다.
- **`/`가 shared mount가 아니다**: docker가 ceph-volume의 `/:/rootfs:rslave`를 거부한다. `mount --make-rshared /`.
- **Cinder 없이도 nova-cell이 cinder keyring을 찾는다**(`Copy over ceph cinder keyring file`의 `.stat` 오류).
  `cinder_backend_ceph: "yes"`, `ceph_cinder_user: "nova"`로 nova keyring을 쓰게 한다. 함께 쓴 값은
  `glance_backend_ceph`·`nova_backend_ceph: "yes"`, `ceph_glance_user: glance`, `ceph_nova_user: nova`이고, keyring과
  `ceph config generate-minimal-conf`(탭 제거)를 `/etc/kolla/config/{glance,nova}/`에 둔다.
- **재부팅 뒤**: loop 장치, shared mount, udevd가 다시 올라와야 한다. 아직 시험하지 않았다(systemd 유닛 예정).
- **메모리**: Ceph와 서비스를 올린 control VM이 6.4 GiB 중 5.2 GiB를 쓰고 여유가 121 MiB였다. `rbd`를 고르면
  control을 8 GiB 이상으로 잡는다.

함께 드러난 것(Ceph와 무관해 보인다):
- 인스턴스로 **ping은 되지만 TCP(22)는 timeout**이었다. SYN은 인스턴스에 닿지만 SYN-ACK가 Neutron OVS 방화벽
  (`firewall_driver = openvswitch`)의 conntrack에서 `+inv`로 판정돼 table 93에서 버려진다. 원인은 checksum offload다.
  virtio 게스트가 TCP checksum을 비워 보내는데 userspace conntrack이 이를 검사해 틀렸다고 본다
  (`ovs-appctl coverage/show`의 `conntrack_l4csum_err`가 시도마다 늘었다). ICMP는 게스트가 checksum을 채워 통과한다.
  **해결**: compute의 OVS에 `ovs-vsctl set Open_vSwitch . other_config:userspace-tso-enable=true`를 넣고
  `openvswitch_vswitchd`를 재시작한 뒤 인스턴스를 hard reboot하면(tap을 다시 만들어야 한다) TCP·SSH가 된다.
  **control(network 노드)에는 넣으면 안 된다**: 세 VM 모두에 넣었더니 인스턴스가 DHCP를 못 받고 ping도 끊겼다.
  control에서만 빼자 DHCP·ping·TCP가 모두 돌아왔다(compute 2대는 켠 채). qrouter·DHCP의 internal 포트와
  `farm1` 링크 중 어디가 깨지는지는 확인하지 않았다.
  - vswitchd를 재시작하면 control의 `qr-`·`qg-`·DHCP `tap` 포트가 DOWN으로 남는다. OVS와 netns에서 포트를 지우고
    `neutron_l3_agent`·`neutron_dhcp_agent`를 재시작하면 다시 만든다. 제품은 인스턴스보다 먼저 설정하므로 해당 없다.
  - 제품(`src/kolla/mod.rs`의 OVS 호환 패치): Kolla의 OVS `other_config` 단계에 넣되 netdev이면서 `compute`
    그룹이고 `network` 그룹이 아닌 노드에만 켠다. verify는 ping 다음에 인스턴스의 TCP 22를 확인한다.
    새 multinode farm(`tso`, control 1 + compute 2) 배포에서 verify의 TCP 검사가 통과했다(2026-11-03). verify의 TCP 22 규칙은
    현재 프로젝트의 `default` 그룹 ID에 넣는다(프로젝트마다 `default`가 생겨 이름만으로는 규칙 생성이 실패한다).
  - **all-in-one farm에는 Nova를 두지 않는다**: compute이면서 network 노드라 TSO를 켤 수 없다. `plan`은 Nova를 고르면
    compute VM을 1대 붙이고(포털도 같이), 배치를 그대로 계산하는 `layout`은 예전 Nova all-in-one 스냅샷 복원과 agent의
    배치 검사에 쓴다. 그래서 Nova farm은 여러 VM 스냅샷이 나올 때까지 새로 얼릴 수 없다.
- evacuate 직후 nova가 `bringing vm to original state: 'stopped'`로 VM을 껐다. event 목록에 stop은 없다. 고립된 c1의
  power state 동기화를 의심한다. `server start`로 켜면 된다.

### 2. 여러 VM 스냅샷

**지금**: `freeze_farm`이 all-in-one VM 하나를 멈추고 msb 디스크 스냅샷 하나(`farm-snap-<id>` 그룹)를 뜬다.
`SnapshotRecord`에는 msb 참조가 하나뿐이고, 내보내기는 `.msb` 아카이브 하나다. multinode farm의 freeze는 막아 두었다.

**바꿀 것**
- **멈추고 뜨는 순서**: compute를 먼저 멈추고 control을 멈춘다(MariaDB·RabbitMQ가 깨끗이 내려가게). 모든 VM을
  같은 그룹에 하나씩 뜬 뒤, control을 먼저 켜고 compute를 켠다.
- **기록**: `SnapshotRecord`가 VM별 참조 목록(role·index → 참조)과 구성(`computes`)을 갖게 한다. 예전 기록은
  참조 하나짜리 all-in-one으로 읽는다.
- **복원**: VM마다 `override_snapshot`으로 만든다. 링크 포트 이름에 farm id가 들어가고 스위치 포트·토큰도 새로
  받으므로, 부팅 뒤 `/etc/farm/link-*.conf`를 새 값으로 바꾸고 `farm-link@` 유닛을 재시작한다(snapshot에서
  만든 VM에는 msb patch를 쓸 수 없다). 그 뒤 기존 settle로 서비스 응답과 public endpoint 이동을 확인한다.
  hostname·관리망 주소는 모든 farm에서 같으므로 그대로 둔다. 장애 설정은 복원할 때 비운다.
- **내보내기·가져오기**: VM별 `.msb`를 하나의 묶음(tar)으로 보내거나, 같은 base 층을 한 번만 담을 수 있는지
  msb의 `--since`·`--with-parents`를 먼저 확인한다. VM마다 `--with-image`를 쓰면 base 층이 VM 수만큼 겹친다.
  S3 내보내기도 이 묶음 하나를 객체 하나로 올린다.
- 포털의 스냅샷 화면과 freeze 버튼이 multinode farm을 받도록 바꾼다.

### 3. 호스트 간 중계

**진행 (2026-11-03)**: 1단계(coordinator 중계)를 제품에 넣었다. 실제 두 호스트(Mac + `dev-01`) 배포는 아직이다.
- 스위치 uplink와 relay(`src/link/relay.rs`), segment 이름을 앞에 붙인 trunk 메시지(`encode_trunk`).
- relay는 coordinator 사설 CA의 TLS로만 연다(`src/link/tls.rs`). CA는 `Create`의 share에 실려 가고 스위치는
  그 CA만 믿는다. uplink 토큰은 (farm, 호스트)마다 32바이트, coordinator에는 해시만 둔다.
- 배치: `placement`, `FarmRecord.shares`, agent의 `Share`와 `Prepare` 작업, 호스트별 메모리 예산.
- MTU는 바꾸지 않았다. 중계 구간이 TCP(WebSocket)라 9000 프레임도 그대로 실린다. 1380은 2단계 UDP 경로에서 필요하다.
- uplink 메시지는 TLS와 별개로 (farm, 호스트) 키로 종단 간 암호화한다(`src/link/seal.rs`). Cloudflare 터널(coordinator가
  관리하는 cloudflared의 임시 터널)이나 프록시가 TLS를 끝내도 프레임을 볼 수 없다.
- 1단계의 relay는 같은 farm의 다른 호스트 모두에 넘긴다(MAC 학습 없음). 호스트가 셋 이상이면 flood가 늘어난다.

**목표**: control은 한 호스트에 두고 compute만 다른 호스트(다른 지역 포함)에 두어, 실제 WAN 지연·손실에서
장애 대응을 시험한다(OpenStack Edge/DCN 패턴). Galera·RabbitMQ가 대륙 간 지연을 견디지 못하므로 control은 나누지 않는다.

**바꿀 것**
- **배치**: plan의 노드마다 `host_id`를 둔다. 지금은 farm 기록에 호스트가 하나다. 생성·삭제·조회를 호스트별
  agent에 나눠 맡기고, 한 호스트라도 실패하면 이미 만든 VM을 정리한다. 메모리 예산도 호스트별로 센다.
- **스위치 사이 trunk**: 스위치에 uplink 포트를 둔다. 이 포트는 여러 segment를 실어 나르므로 메시지에
  segment 식별자를 붙인다. MAC 학습은 uplink 너머의 MAC도 그 포트로 배운다.
- **경로 1단계 (coordinator 중계)**: 각 호스트의 스위치가 coordinator의 새 WebSocket 엔드포인트
  (예: `/api/v1/link-relay`)에 호스트 토큰으로 붙고, coordinator가 segment별로 프레임을 전달한다. 링크를
  WebSocket으로 바꾼 이유가 이 구간이다. SSH 호스트의 agent는 요청마다 뜨는 프로세스라 지속 연결이 없으므로,
  연결은 agent가 아니라 스위치 프로세스가 맡는다.
- **경로 2단계 (직접 연결)**: 서로 닿는 호스트끼리는 스위치끼리 `wss://`로 바로 잇고, 이어서 UDP(QUIC datagram
  또는 WireGuard 방식)와 NAT hole punching을 붙인다. 실패하면 coordinator 중계로 돌아간다.
- **MTU**: 호스트 안은 9000이지만 WAN을 지나는 farm은 1380 정도로 낮추고 Kolla `global_physnet_mtu`도 맞춘다.
  한 farm의 링크 MTU는 같아야 하므로 생성할 때 배치를 보고 정한다.
- **Kolla**: control VM에서 원격 compute로의 SSH와 nova-compute → RabbitMQ 연결이 모두 관리망 링크로 가므로
  inventory는 지금과 같다.

**먼저 확인할 것**: 이 Mac(control, compute-1)과 Linux 서버 하나(compute-2)로 farm을 만들고, coordinator 중계를
거쳐 배포·evacuate가 되는지, 처리량과 RTT가 얼마인지.

### 남은 검증

- ~~multinode farm을 **원격 Linux 호스트**에서 만들고 배포~~: `dev-01`에서 control 1 + compute 1 farm `dev-001`이
  Ready까지 갔다(2026-11-03).
- ~~**nested KVM**~~: M5 Pro에서 compute VM에 `/dev/kvm`이 있고 `virt_type = kvm`으로 배포됐다(2026-11-03). M1 Max는 TCG였다.
  Linux/KVM 호스트는 아직이다.
- multinode farm의 **VM 재시작 뒤 settle**을 실제 VM으로 돌려보지 않았다. fake backend 테스트만 있다.
  **삭제**는 실제 `mn` farm으로 확인했다: `farm delete mn`이 2초 만에 VM 3대와 스위치 포트 6개를 정리했다.
- Horizon을 넣은 multinode farm은 배포해 보지 않았다.
- 3단계 변경 뒤 Linux 컨테이너에서 clippy를 다시 돌리지 않았다(Linux 전용 TAP 코드는 바꾸지 않았다).

## 다른 머신에서 이어가기 (회사 Apple M5 Pro)

```bash
git fetch origin && git switch poc/l2-link && git pull
task dev        # farm·Linux farm-agent(Docker 필요)·포털을 빌드하고 daemon과 포털을 띄운다
```

- `task dev`의 agent 빌드가 `dist/agent/aarch64/farm-agent`를 만든다. macOS coordinator는 시작할 때 이것을
  multinode 게스트용으로 넘기므로, agent를 다시 빌드했으면 coordinator도 다시 띄운다.
- golden image는 예전 `farm-golden:rocky10`이 아니라 msb 스냅샷 `farm-golden-rocky10:<해시>`를 쓴다. 첫 farm을
  만들 때 자동으로 빌드되고(약 35초), `task golden`으로 미리 만들 수 있다. 예전 이미지로 만든 farm은 그대로 동작한다.
- 메모리: control 1 + compute 1은 약 9.5 GiB, compute 2는 약 12.5 GiB다. 24 GiB 머신의 기본 예약 한도(80%,
  약 19 GiB)에서는 다른 farm을 끄고 만드는 편이 안전하다.
- M5 Pro는 msb nested virtualization이 되므로 "남은 검증"의 nested KVM 항목(compute VM의 `virt_type=kvm`)을
  여기서 확인할 수 있다. 배포 로그의 configure 단계와 compute VM의 `/dev/kvm`을 본다.
- 포털: **Farm 만들기 → VM 구성**에서 Control 1 + Compute N을 고르고, 배포 뒤 상세 화면 topology의
  고립·복구 버튼으로 evacuate를 시험한다. evacuate 명령 자체는 control VM 셸(Shell 탭)에서
  `compute service set --down`, `server evacuate --host farm-compute-2`로 한다.

## PoC 재현

PoC 스크립트와 `src/bin/farm-link.rs`는 `71594d0`에 있다. msb CLI로 systemd VM을 만들어야 해서 Docker로 만든 예전
이미지를 쓴다(msb CLI의 restore에는 init을 지정하는 옵션이 없다).

```bash
git worktree add /tmp/farm-poc 71594d0 && cd /tmp/farm-poc
task docker-build                      # 예전 farm-golden:rocky10을 Docker로 만들고 msb에 load
sh scripts/poc-l2-link.sh up && sh scripts/poc-l2-link.sh bench && sh scripts/poc-l2-link.sh down
sh scripts/poc-multinode.sh up         # VM 3대(약 12 GiB), 링크 6개
sh scripts/poc-multinode.sh deploy     # kolla-ansible 설치와 multinode 배포 (약 35분)
sh scripts/poc-multinode.sh evacuate   # c1에 부팅, c1 고립, c2로 evacuate
sh scripts/poc-multinode.sh down
```

스크립트는 aarch64 게스트를 가정한다(`--platform linux/arm64`). x86_64 Linux 호스트라면 `linux/amd64`로 바꾼다.
