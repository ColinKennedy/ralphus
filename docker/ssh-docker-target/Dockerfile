# A "remote host" that has its own Docker engine: docker-in-docker plus sshd.
# The Ralphus daemon stays on the real host and reaches this container through
# the SSH provider; the provider then creates and enters a work container on
# *this* engine, so the whole daemon -> ssh -> docker exec chain is real.
FROM docker:dind

RUN apk add --no-cache openssh-server bash git shadow \
    && adduser -D -u 10002 -s /bin/bash ralphus \
    && addgroup ralphus docker \
    && sed -i 's/^ralphus:!/ralphus:*/' /etc/shadow \
    && install -d -m 0700 -o ralphus -g ralphus /home/ralphus/.ssh \
    && install -d -m 0755 /run/sshd /bootstrap

COPY docker/ssh-docker-target/sshd_config /etc/ssh/sshd_config
COPY docker/ssh-docker-target/entrypoint.sh /usr/local/bin/ralphus-ssh-docker-target-entrypoint
RUN chmod 0755 /usr/local/bin/ralphus-ssh-docker-target-entrypoint

EXPOSE 22
ENTRYPOINT ["/usr/local/bin/ralphus-ssh-docker-target-entrypoint"]
