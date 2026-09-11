FROM python:3.12-slim

# 国内镜像加速（云主机 ECS 访问 pypi 官方源较慢）
ENV PIP_INDEX_URL=https://mirrors.aliyun.com/pypi/simple/ \
    PIP_TRUSTED_HOST=mirrors.aliyun.com \
    PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONPATH=/app/src \
    PLAYWRIGHT_BROWSERS_PATH=/ms-playwright

# STATUS_IMAGE=true 时才装 Chromium（/status 发图用，镜像会大 1GB 左右）
ARG STATUS_IMAGE=false

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

RUN if [ "$STATUS_IMAGE" = "true" ]; then \
        apt-get update && apt-get install -y --no-install-recommends fonts-noto-cjk && rm -rf /var/lib/apt/lists/* && \
        pip install --no-cache-dir playwright && \
        playwright install --with-deps chromium && \
        rm -rf /var/lib/apt/lists/* ; \
    fi

COPY src ./src
COPY tests ./tests
RUN mkdir -p /data

EXPOSE 6199

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD python -c "import urllib.request;urllib.request.urlopen('http://127.0.0.1:6199/healthz',timeout=3)"

CMD ["python", "-m", "chatroom_bridge.main", "--config", "/app/config.json"]
