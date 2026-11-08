# Hindsight v1.2.0 API, built from the pinned PyPI release.
#
# Why this exists instead of `docker pull ghcr.io/vectorize-io/hindsight:1.2.0`:
# on the machine the published numbers were taken on, Docker refused every
# ghcr.io pull ("denied: denied", also for unrelated public images and with an
# empty client config). This image installs the same release (hindsight-api
# 1.2.0, the package the official image is built from, with its local-ml and
# embedded-db extras) so the service under test is the same code at the same
# version. If ghcr.io works for you, prefer the official image; see README.
FROM python:3.11-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN pip install --no-cache-dir "hindsight-api==1.2.0"

# The official image runs as a non-root user and keeps its embedded Postgres here.
RUN useradd --create-home --uid 1000 hindsight
USER hindsight
WORKDIR /home/hindsight
ENV HINDSIGHT_API_HOST=0.0.0.0 HINDSIGHT_API_PORT=8888
EXPOSE 8888
CMD ["hindsight-api"]
