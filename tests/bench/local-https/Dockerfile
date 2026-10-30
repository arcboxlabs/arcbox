FROM arcbox-validation-musl:1.96.0

LABEL org.arcbox.validation.base-image-id="sha256:766c0dfb539f26ef7c15251a25a6874a658103301b7ec2aea53c9b4adf78a0e7"

RUN apk add --no-cache iptables=1.8.13-r0 iptables-legacy=1.8.13-r0 iproute2=7.0.0-r0

RUN ln -sf iptables-legacy /usr/sbin/iptables \
    && ln -sf iptables-legacy-save /usr/sbin/iptables-save \
    && ln -sf iptables-legacy-restore /usr/sbin/iptables-restore \
    && ln -s /usr/sbin/iptables /sbin/iptables \
    && ln -s /usr/sbin/iptables-save /sbin/iptables-save \
    && ln -s /usr/sbin/iptables-restore /sbin/iptables-restore
