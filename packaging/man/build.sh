#!/bin/sh
# Regenerate apprise.1 and apprise.1.html from apprise.md with Ronn-NG, run
# in a container so no local Ruby is needed. The date comes from the last
# commit of apprise.md, so the output only changes when the source does.
set -eu
cd "$(dirname "$0")"
date=$(git log -1 --format=%cs -- apprise.md)
docker run --rm -v "$PWD":/man -w /man ruby:3.1-alpine sh -c "
  gem install --no-document -q ronn-ng -v 0.9.1 >/dev/null &&
  ronn --roff --html --date=$date --organization='Chris Caron <lead2gold@gmail.com>' apprise.md"
