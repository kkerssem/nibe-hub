#!/bin/sh
# Builds the arm64 image and pushes it to a registry, for a MikroTik App (which pulls images
# from a registry; it cannot import a tar file).
#
#   tools/push_image.sh            # pushes ghcr.io/kkerssem/nibe-hub:<commit> and :latest
#   IMAGE_REPO=ghcr.io/you/nibe-hub tools/push_image.sh
#
# Log in first (a GitHub token with the write:packages scope):
#   container registry login ghcr.io
# A new package on ghcr.io is private. RouterOS Apps document no registry login, so make the
# package public on GitHub (Package settings -> Change visibility); the image contains only the
# two binaries and CA certificates, no configuration or secrets.
set -eu

IMAGE_REPO=${IMAGE_REPO:-ghcr.io/kkerssem/nibe-hub}
cd "$(dirname "$0")/.."

if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "the working tree has uncommitted changes; commit first so the tag matches the code" >&2
    exit 1
fi
TAG=$(git rev-parse --short=12 HEAD)

container build --platform linux/arm64 -t "nibe-hub:$TAG" .
for t in "$TAG" latest; do
    container image tag "nibe-hub:$TAG" "$IMAGE_REPO:$t"
    container image push --platform linux/arm64 "$IMAGE_REPO:$t"
done
echo "pushed $IMAGE_REPO:$TAG and $IMAGE_REPO:latest"
echo "for a fixed version in the app: python3 tools/make_app_yaml.py --image $IMAGE_REPO:$TAG ..."
