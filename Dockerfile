ARG BUILD_FROM=ghcr.io/home-assistant/base:3.23

FROM ${BUILD_FROM}
COPY run.sh /run.sh
COPY dist/energy-planner /usr/bin/energy-planner

# Binary is built on Ubuntu runners (glibc); add compatibility/runtime libs on Alpine-based HA images.
RUN if command -v apk >/dev/null 2>&1; then \
			apk add --no-cache gcompat libstdc++; \
		fi

RUN chmod a+x /run.sh /usr/bin/energy-planner

CMD ["/run.sh"]
