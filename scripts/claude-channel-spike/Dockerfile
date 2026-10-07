FROM ubuntu:22.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends python3 ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -s /bin/bash ada
COPY claude /usr/local/bin/claude
COPY key_helper.py mcp.py pty_probe.py /opt/spike/
RUN chmod 755 /usr/local/bin/claude && mkdir -p /home/ada/work /home/ada/results \
    && chown -R ada:ada /home/ada
USER ada
WORKDIR /home/ada/work
CMD ["sleep", "infinity"]
