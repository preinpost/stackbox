# PoC: WebSocket으로 VM 간 L2 잇기 (2026-11-02)

msb는 VM끼리 트래픽을 중계하지 않는다. 역할별 VM(control, compute)을 한 farm으로 묶으려면
VM 사이에 L2가 필요하다. 이 PoC는 VM 안의 TAP을 WebSocket으로 호스트의 스위치에 연결해
그 L2를 만들고, OpenStack 내부망으로 쓸 만한지 쟀다.

```
[farm-poc-a] farm0(TAP, 10.254.0.2) ⇄ farm-link link ─ws─┐
                                                          ├─ farm-link switch (호스트, MAC 학습)
[farm-poc-b] farm0(TAP, 10.254.0.3) ⇄ farm-link link ─ws─┘
```

- `src/bin/farm-link.rs`: `switch`(호스트), `link`(게스트, TAP ⇄ WebSocket), `sink`/`send`(기준 측정).
  WebSocket 메시지 하나가 Ethernet 프레임 하나다. 일괄 전송 같은 최적화는 하지 않았다.
- 스위치는 포트마다 지연과 손실을 넣는다(`POST /impair?name=b&delay_ms=50&loss=0.01`).
  지연은 그 포트로 들어가고 나가는 양방향에 걸리므로 RTT는 두 배만큼 늘어난다.
- msb 네트워크 코드에는 VM에 두 번째 NIC를 소켓으로 붙이는 기능이 없다. 그래서 L2는 게스트 안의
  TAP으로 만든다. 게스트 커널(6.12.113)에 `/dev/net/tun`이 있다. 게스트는 호스트의 loopback에
  `host.microsandbox.internal`로 닿으므로 스위치는 외부 포트를 열지 않는다.
- 재현: `sh scripts/poc-l2-link.sh up`, `bench`, `down`.

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
2. **(다음 할 일)** 스위치를 farm-agent로 옮기고, VM별 토큰으로 링크를 인증한다. 프레임을 묶어 보내 MTU 1500의
   처리량을 높인다. golden image에 `farm-link`와 systemd unit을 넣는다.
3. farm 모델에 노드 구성(control/compute 수)과 공유 스토리지 선택을 넣는다. plan, 메모리 예산,
   배포 단계, 포탈의 노드·토폴로지 화면을 바꾼다.
4. 여러 VM 스냅샷.
5. 호스트 간 중계, 이어서 UDP 직접 연결.

## multinode와 evacuate (2026-11-02, Apple M1 Max 32 GiB)

`sh scripts/poc-multinode.sh up`, `deploy`, `evacuate`로 재현한다. 게스트에서 도는 파일은
`scripts/poc-multinode/`에 있다.

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
- golden image의 `authorized_keys`는 끝에 개행이 없다. 키를 덧붙이면 앞 키와 한 줄로 붙는다.
- `msb copy`는 실행 권한을 남기지 않는다. 복사 뒤 `chmod`한다.
- 게스트 hostname은 VM마다 달라야 한다(`mn-ctl` 등). 지금의 고정 `farm-aio`는 multinode에서 쓸 수 없다.

## 다른 머신에서 이어가기

```bash
git fetch origin && git switch poc/l2-link
task docker-build                 # farm-golden:rocky10을 Docker로 만들고 msb에 load
sh scripts/poc-l2-link.sh up      # farm-link 빌드, VM 2대, 스위치와 링크
sh scripts/poc-l2-link.sh bench   # 위 표와 비교
sh scripts/poc-l2-link.sh down

sh scripts/poc-multinode.sh up         # VM 3대(약 12 GiB), 링크 6개
sh scripts/poc-multinode.sh deploy     # kolla-ansible 설치와 multinode 배포 (약 35분)
sh scripts/poc-multinode.sh evacuate   # c1에 부팅, c1 고립, c2로 evacuate
sh scripts/poc-multinode.sh heal c1
sh scripts/poc-multinode.sh down
```

- 필요한 것: msb 0.7.13, Docker(게스트용 Linux 바이너리 빌드), Rust, 여유 메모리.
- 스크립트는 aarch64 게스트를 가정한다(`--platform linux/arm64`). x86_64 Linux 호스트라면
  `linux/amd64`로 바꾼다.
- 결과 숫자는 Apple M5 Pro 기준이다. 다른 머신에서는 기준 측정(`sink`/`send`)부터 다시 잰다.
