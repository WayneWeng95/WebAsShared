# RMMap (dmerge) WordCount — ES protocol, single-box image.
#
# Built on STOCK python:3.7 (NOT RMMap's private custom-CPython fork, which is
# only needed for DMERGE's heap-malloc path). The dmerge `bindings` Cython module
# compiles and imports fine here without the kernel module; for ES its syscalls
# (sopen/pull/push) are never called. This bypasses the private val01:5000 base
# images and the SJTU-internal cpython repo.
FROM python:3.7-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY app/ /app/

# Build the dmerge Cython bindings (native/wrapper.h is self-contained C).
RUN pip install --no-cache-dir cython redis requests flask && \
    python setup.py build_ext --inplace && \
    python -c "import bindings; print('bindings OK')"

COPY driver.py /app/driver.py

ENV PROTOCOL=ES
ENTRYPOINT ["python", "driver.py"]
