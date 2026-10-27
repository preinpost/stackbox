# Farm 단위 호스트 배치

현재 실행 단위는 **Farm당 all-in-one VM 1대**다. 사용자는 로컬 Coordinator 또는 등록한
원격 Linux 호스트 중 하나를 선택하고, 선택한 OpenStack 컴포넌트를 그 VM에 함께 설치한다.
같은 Farm의 컴포넌트를 여러 호스트로 나누던 이전 설계는 현재 구현 방향에서 제외했다.

1. 호스트 IP·SSH 사용자·비밀번호 또는 키를 등록하고 host key 지문을 확인한다.
2. 호스트의 msb·KVM·이미지·실행기를 점검하고 필요한 준비 작업을 수행한다.
3. 컴포넌트 의존성을 계산하고 단일 VM의 메모리·디스크와 호스트 공개 포트를 예약한다.
4. VM `<farm>`을 생성하고 VM 안에서 Kolla all-in-one inventory와 로컬 Ansible로 배포한다.
5. 선택 서비스의 native 포트를 호스트로 전달하고 인증·API·이미지 왕복을 검증한다.

호스트별 메모리와 포트 예약, SSH 핀 고정, 암호화 인증정보, 작업 로그와 실패 예약은 기존
관리 계층을 사용한다. Farm 전체를 서로 다른 호스트에 여러 개 실행할 수 있지만 Farm 내부
VM 간 VXLAN·WireGuard·NIC 추가는 필요하지 않다. Neutron tenant network와 외부 veth는
그 VM 내부의 OpenStack 네트워크이며 호스트 간 Farm 연결과는 별개다.

기존 다중 VM Farm은 자동 변환하거나 삭제하지 않는다. 조회·인증정보 조회·삭제를 유지하고
재배포는 거절한다. 필요한 데이터를 보관한 뒤 별도 단일 VM Farm을 생성한다.
원격 실행기 업그레이드와 운영 조건은 [remote-hosts.md](remote-hosts.md)를 참고한다.
