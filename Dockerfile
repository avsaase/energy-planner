ARG BUILD_FROM=ghcr.io/hassio-addons/debian-base/amd64:latest

FROM ${BUILD_FROM}
COPY run.sh /run.sh
COPY dist/energy-planner /usr/bin/energy-planner

RUN chmod a+x /run.sh /usr/bin/energy-planner

CMD ["/run.sh"]
