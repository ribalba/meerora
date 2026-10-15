# meerora.com

The website of meerora: one static page with the demo video in the middle,
served by nginx in a container. It sets no cookies and loads nothing from
other servers. Fonts, video and pictures are served from the container.

| Path | What it is |
| --- | --- |
| `site/` | The page (`index.html`, `style.css`, `main.js`, `404.html`) and its media |
| `site/media/` | The video as MP4 and AV1 WebM, the poster, the feature pictures and the social card |
| `site/install.sh` | The install script behind `curl -fsSL https://meerora.com/install.sh \| sh` (see below) |
| `build_media.py` | Builds `site/media`, `site/icons` and `site/fonts` from a render of the video, `assets/logo` and `assets/fonts` |
| `nginx.conf` | Caching, security headers and the 404 page |
| `Dockerfile` | nginx 1.30 on Alpine with the page |
| `docker-compose.yml` | The service for Coolify |

## New video

Render the video with `docs/video/make-video.sh` and build the media from it
and from the stills of its work folder:

```bash
docs/video/make-video.sh --work /tmp/video --kokoro-python <venv>/bin/python
python3 website/build_media.py --video ~/Videos/meerora-demo-N.mp4 --stills /tmp/video/stills
```

The AV1 version takes a few minutes. `--no-webm` skips it, and browsers then
play the MP4. The video files are large (about 55 MB for the MP4), so every
new render adds that much to the git history.

## Try it locally

```bash
cd website
docker build -t meerora-web .
docker run --rm -p 127.0.0.1:8080:80 meerora-web
```

Then open http://127.0.0.1:8080.

## Deploy with Coolify

1. Commit `website/` with its media and push it.
2. In Coolify, add a new resource from the repository, the public one or one
   with a GitHub app or deploy key, and pick the branch.
3. Choose the build pack **Docker Compose**, with **Base Directory** `/website`
   and **Docker Compose Location** `/docker-compose.yml`.
4. Under the web service's domains, enter `https://meerora.com` (and
   `https://www.meerora.com` if you want it). Coolify's proxy routes it to port
   80 of the container and gets the certificate from Let's Encrypt. Until then
   the service answers on a generated URL of the server's wildcard domain.
5. Point the DNS records of the domains at the Coolify server and deploy.

The container reports healthy once nginx answers on port 80. Coolify builds
the image on the server, so a deploy only needs the repository.

## Install script and releases

`site/install.sh` downloads `meerora-x86_64-linux.tar.gz` and its `.sha256`
from the latest release of [ribalba/meerora](https://github.com/ribalba/meerora), checks the
SHA-256 and installs meerora for the current user. nginx serves it as plain
text and never caches it. It never runs sudo. For missing packages such as
bubblewrap it prints the dnf, apt, pacman or zypper command. Running it again
updates in place.

| Path | What the script puts there |
| --- | --- |
| `~/.local/share/meerora-app/<version>/` | The unpacked tarball. `current` links to the new version, and the one before it stays until the next update |
| `~/.local/bin/meerora`, `meerora-agentd`, `meerora-mcp` | Links to `current/bin/`. Files or links it did not make stay as they are |
| `~/.local/share/applications/meerora.desktop` | The menu entry with `Exec=<app folder>/current/bin/meerora --windowed` |
| `~/.local/share/icons/hicolor/<N>x<N>/apps/meerora.png` | The icon |
| `~/.local/share/meerora/` | The data dir from `meerora-agentd init`, only when there is none yet |
| `~/.local/state/meerora/meerora.log` | The output of the meerora it starts at the end |

`UNINSTALL.txt` in the app folder lists what to remove. `MEERORA_VERSION=v0.1.0`
installs a given tag, `MEERORA_NO_LAUNCH=1` skips the start and
`MEERORA_TARBALL` takes a local tarball or URL instead of the download. When
there is no release, the script asks whether to build from source.
`--from-source` does that right away. It clones into
`~/.local/share/meerora-app/source` and builds with `scripts/package-release.sh`,
which takes 10 to 20 minutes.

### Cut a release

```bash
git tag -a v0.1.0 -m "meerora 0.1.0"
git push origin v0.1.0
```

`.github/workflows/release.yml` builds the three binaries on Ubuntu 24.04,
packs them with `scripts/package-release.sh` and attaches the tarball and its
`.sha256` to the release of the tag, which it creates when there is none. A tag
with a hyphen, such as `v0.2.0-rc1`, becomes a pre-release, which the install
script skips. To build an existing tag again, run the workflow by hand under
Actions with the tag as input.

### What the tarball contains

```text
meerora-x86_64-linux/
  VERSION, README.txt
  bin/meerora, bin/meerora-agentd, bin/meerora-mcp
  share/meerora/           spaces, agents and scenarios from assets/, and licenses/
  share/icons/hicolor/     the logo in 7 sizes
```

The binaries find `share/meerora` next to their `bin/` folder, as after an
install to `/usr/local`, and the app starts `meerora-agentd` from its own
folder. Ubuntu 24.04 has glibc 2.39, so the binaries run on Ubuntu 24.04,
Fedora 40 and Debian 13 or newer. whisper.cpp is built with `GGML_NATIVE=OFF`,
so it does not depend on the CPU of the build machine, but voice needs AVX2.
The binaries link libstdc++, libgcc_s, libc and libxkbcommon. At run time they
load libwayland-client (or the X11 libraries) and Vulkan or EGL.

### Test the script locally

Build a tarball in a target dir of its own and install it in a throwaway
container as a normal user:

```bash
CARGO_TARGET_DIR=$PWD/target/dist scripts/package-release.sh --out /tmp/meerora-dist
docker run --rm --user 1000:1000 -e HOME=/tmp/home \
  -e MEERORA_TARBALL=/dist/meerora-x86_64-linux.tar.gz -e MEERORA_NO_LAUNCH=1 \
  -v /tmp/meerora-dist:/dist:ro -v "$PWD/website/site/install.sh:/install.sh:ro" \
  fedora:43 sh -c 'mkdir -p "$HOME" && sh /install.sh'
```

Check the scripts with `sh -n` and `shellcheck -s sh website/site/install.sh
scripts/package-release.sh`. The download itself can only be tried once a
release exists, with `curl -fsSL https://meerora.com/install.sh | MEERORA_NO_LAUNCH=1 sh`.
