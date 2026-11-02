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

OpenStack 내부망(RabbitMQ, DB, API, Neutron VXLAN 터널, 이미지 전송)으로 충분하다. 다음 단계는
control VM과 compute VM으로 Kolla multinode를 배포해 evacuate를 해 보는 것이다.
남은 일: 스위치를 farm-agent 안으로 옮기기, VM별 인증 토큰, 프레임 일괄 전송, golden image에
`farm-link` 넣기, 호스트 간 중계.
