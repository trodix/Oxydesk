# Real RDP clients (xfreerdp, Remmina) driven in a virtual X server, to test oxydesk without installing
# anything on the development machine. Docker and Podman build and run it the same way (ci/clients-test.sh).
FROM debian:trixie-slim
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
        xvfb xauth xdotool x11-utils xclip imagemagick openbox dbus-x11 procps python3 python3-pil \
        freerdp3-x11 remmina remmina-plugin-rdp ca-certificates cups-daemon cups-client \
    && rm -rf /var/lib/apt/lists/*
COPY probe.py run.sh /opt/clients/
RUN chmod +x /opt/clients/run.sh
ENTRYPOINT ["/opt/clients/run.sh"]
