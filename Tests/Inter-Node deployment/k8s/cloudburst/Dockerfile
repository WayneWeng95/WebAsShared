# Cloudburst executor image — a lightweight worker that runs the real WordCount /
# TeraSort / FINRA / Matrix stage bodies (wc_ops / ts_ops / finra_rules / numpy einsum)
# and routes inter-stage state through Redis. One per pod; the Deployment spreads a pool
# across the 4 nodes.
FROM python:3.11-slim
RUN pip install --no-cache-dir redis==5.0.8 cloudpickle==3.0.0 numpy==2.4.4
WORKDIR /app
COPY wc_ops.py ts_ops.py finra_rules.py sgd_core.py infer_core.py executor.py /app/
ENV REDIS_HOST=redis.baselines.svc.cluster.local \
    REDIS_PORT=6379 \
    CB_TASK_QUEUE=cb:tasks \
    PYTHONUNBUFFERED=1
CMD ["python", "executor.py"]
