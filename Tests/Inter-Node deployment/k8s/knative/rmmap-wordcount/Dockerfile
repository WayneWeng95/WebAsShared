# RMMap mapper Knative-Service image. Named under the `dev.local/` prefix so
# Knative's webhook SKIPS digest tag-resolution (dev.local is in the default
# registries-skipping-tag-resolving list) — the image is imported into every
# node's containerd (k3s ctr import), there is no registry to resolve against.
FROM python:3.11-slim
RUN pip install --no-cache-dir redis==5.0.8 flask==3.0.3
WORKDIR /app
COPY wc_ops.py mapper_app.py /app/
ENV REDIS_HOST=redis.baselines.svc.cluster.local \
    REDIS_PORT=6379 \
    PYTHONUNBUFFERED=1
# Knative injects $PORT (default 8080); gunicorn keeps the worker robust under the
# activator's request buffering. containerConcurrency=1 in the ksvc keeps it 1:1.
RUN pip install --no-cache-dir gunicorn==22.0.0
CMD ["sh", "-c", "exec gunicorn -w 1 -t 600 -b 0.0.0.0:${PORT:-8080} mapper_app:app"]
