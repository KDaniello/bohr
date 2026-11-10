# bohr in a container: the static release binary and its target packs on Debian slim.
#
# Built from a release's files, so the image runs the binary the release signed. Put `bohr`,
# `bohr-packs-<version>.tar.gz` and `scripts/docker-help.txt` beside this file, then
#
#   docker build --build-arg VERSION=v1.0.0 -t ghcr.io/kdaniello/bohr:v1.0.0 .
#
# The base has glibc and a dynamic loader, which most programs bohr runs need.
FROM debian:12.8-slim

ARG VERSION
LABEL org.opencontainers.image.title="bohr" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.source="https://github.com/KDaniello/bohr" \
      org.opencontainers.image.description="Deterministic simulation testing. Needs: docker run --cap-add SYS_PTRACE --security-opt seccomp=unconfined --security-opt apparmor=unconfined" \
      org.opencontainers.image.licenses="Apache-2.0"

COPY bohr /usr/local/bin/bohr
COPY bohr-packs-${VERSION}.tar.gz /tmp/packs.tar.gz
RUN mkdir -p /root/.bohr/packs \
 && tar -xzf /tmp/packs.tar.gz -C /root/.bohr/packs \
 && rm /tmp/packs.tar.gz \
 && chmod 755 /usr/local/bin/bohr

COPY docker-help.txt /usr/share/doc/bohr/docker.txt
WORKDIR /work
ENTRYPOINT ["/usr/local/bin/bohr"]
# With no arguments the image runs `bohr doctor`, which names any missing container flag.
CMD ["doctor"]
