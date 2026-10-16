FROM rockylinux/rockylinux:10

# systemd 부팅, kolla-ansible 배포 대상/배포 노드에 필요한 기본 패키지
# Docker는 kolla-ansible bootstrap-servers가 설치
RUN dnf install -y \
      systemd dbus-broker \
      openssh-server openssh-clients sudo \
      python3 python3-pip python3-libselinux git \
      iproute iputils procps-ng hostname which chrony tar && \
    dnf clean all && \
    systemctl enable sshd chronyd && \
    mkdir -p /root/.ssh && chmod 700 /root/.ssh

# msb가 게스트에 IPv6 주소를 주지만 호스트에 IPv6 경로가 없으면 연결이 끊김 → IPv4만 사용
# (이미지에 systemd-sysctl이 없어서 부팅 시 직접 적용)
COPY disable-ipv6.service /etc/systemd/system/disable-ipv6.service
RUN systemctl enable disable-ipv6

# msb는 VM 간 트래픽을 중계하지 않음 → farm 노드는 호스트의 published UDP 포트로 VXLAN mesh(vx0)를 구성
# 설정(/etc/farm/overlay.conf)은 farm 생성 시 주입되고, 없으면 유닛이 실행되지 않음
COPY --chmod=755 farm-overlay /usr/local/sbin/farm-overlay
COPY farm-overlay.service /etc/systemd/system/farm-overlay.service
RUN mkdir -p /etc/farm && systemctl enable farm-overlay

# 호스트에서 접속할 SSH 공개키 (farm 내부 배포용 키는 farm 생성 시 주입)
COPY authorized_keys /root/.ssh/authorized_keys
RUN chmod 600 /root/.ssh/authorized_keys

CMD ["/sbin/init"]
