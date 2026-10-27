FROM rockylinux/rockylinux:10

# systemd 부팅, kolla-ansible 배포 대상/배포 노드에 필요한 기본 패키지
# Docker는 kolla-ansible bootstrap-servers가 설치
RUN dnf install -y \
      systemd dbus-broker \
      openssh-server openssh-clients sudo \
      python3 python3-pip python3-libselinux git \
      iproute iputils procps-ng hostname which chrony tar nftables && \
    dnf clean all && \
    systemctl enable sshd chronyd && \
    mkdir -p /root/.ssh && chmod 700 /root/.ssh

# msb가 게스트에 IPv6 주소를 주지만 호스트에 IPv6 경로가 없으면 연결이 끊김 → IPv4만 사용
# (이미지에 systemd-sysctl이 없어서 부팅 시 직접 적용)
COPY disable-ipv6.service /etc/systemd/system/disable-ipv6.service
RUN systemctl enable disable-ipv6

# Pin the guest hostname and service address, then restore msb's generated
# hostname mapping before Docker/RabbitMQ start.
COPY --chmod=755 farm-identity /usr/local/sbin/farm-identity
COPY farm-identity.service /etc/systemd/system/farm-identity.service
COPY --chmod=755 farm-hostname /usr/local/sbin/farm-hostname
COPY farm-hostname.service /etc/systemd/system/farm-hostname.service
COPY --chmod=755 farm-external /usr/local/sbin/farm-external
COPY farm-external.service /etc/systemd/system/farm-external.service
RUN mkdir -p /etc/farm && systemctl enable farm-identity farm-hostname farm-external

# Optional host SSH public key; all-in-one Kolla runs locally without peer SSH.
COPY authorized_keys /root/.ssh/authorized_keys
RUN chmod 600 /root/.ssh/authorized_keys

CMD ["/sbin/init"]
