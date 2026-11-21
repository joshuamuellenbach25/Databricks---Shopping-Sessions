#!/bin/sh
# Runs the solver image with the scorer's container flags, reading a dataset
# from a Docker volume so that file access stays inside the Linux VM.
# Usage: scripts/docker_volume_run.sh DATA_VOLUME OUTPUT_VOLUME [IMAGE]
# CPUS and MEM override the 8 CPUs and 24g memory limit. SOLVER_ENV adds
# space-separated NAME=VALUE pairs to the container environment.
set -eu
export MSYS_NO_PATHCONV=1
DATA=$1
OUT=$2
IMAGE=${3:-shopping-sessions:latest}
CPUS=${CPUS:-8}
MEM=${MEM:-24g}
docker run --rm -v "$OUT":/output alpine:3.20 sh -c 'find /output -mindepth 1 -delete; chown 1000:1000 /output' > /dev/null
set --
for pair in ${SOLVER_ENV:-}; do
  set -- "$@" -e "$pair"
done
started=$(date +%s%N)
status=0
docker run --rm --platform linux/amd64 "$@" \
  --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 \
  --cap-drop ALL --security-opt no-new-privileges \
  --pids-limit 4096 --cpus "$CPUS" --memory "$MEM" --memory-swap "$MEM" \
  --user 1000:1000 \
  -v "$DATA":/data:ro -v "$OUT":/output --tmpfs /scratch:rw,size=1g,mode=1777 \
  -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch \
  -e NCPU="${CPUS%.*}" -e MEM_GB="${MEM%g}" \
  "$IMAGE" || status=$?
finished=$(date +%s%N)
echo "container wall: $(( (finished - started) / 1000000 )) ms, exit $status" >&2
exit "$status"
