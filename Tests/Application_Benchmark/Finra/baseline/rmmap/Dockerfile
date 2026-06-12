# RMMap (dmerge) FINRA — ES protocol, single-box image (stock python:3.7).
# ES needs no kernel module / dmerge bindings (pure Redis+pickle), so this is a
# minimal image running the frozen 8-rule spec over Redis with parallel rules.
FROM python:3.7-slim
WORKDIR /app
RUN pip install --no-cache-dir redis
COPY finra_rules.py driver.py /app/
ENTRYPOINT ["python", "driver.py"]
