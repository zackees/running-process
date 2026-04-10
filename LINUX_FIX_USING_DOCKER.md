# Linux Fix Using Docker

## Target Shape

The Linux path is now intentionally simple:

1. `script -> docker build cached builder image -> docker run export -> dist-dev/`
2. `docker-pytest -> copy dist-dev/*.whl -> install -> pytest`

That means there are only two Dockerfiles:

- [Dockerfile.linux-build](/C:/Users/niteris/dev/running-process/Dockerfile.linux-build)
- [Dockerfile.linux-pytest](/C:/Users/niteris/dev/running-process/Dockerfile.linux-pytest)

Both are Alpine-based.

## Files

- [Dockerfile.linux-build](/C:/Users/niteris/dev/running-process/Dockerfile.linux-build)
- [Dockerfile.linux-pytest](/C:/Users/niteris/dev/running-process/Dockerfile.linux-pytest)
- [ci/linux_docker.py](/C:/Users/niteris/dev/running-process/ci/linux_docker.py)
- [ci/linux_build_wheel.py](/C:/Users/niteris/dev/running-process/ci/linux_build_wheel.py)
- [ci/linux_pytest.py](/C:/Users/niteris/dev/running-process/ci/linux_pytest.py)

## Builder Image

The builder image is stable and contains only toolchain pieces:

- `python:3.11-alpine`
- `bash`
- `build-base`
- `cargo`
- `rust`
- `git`
- `pkgconf`
- `maturin`

It selectively copies only the build-relevant repo files into the image.

The build flow is:

- `docker build` the cached builder image from [Dockerfile.linux-build](/C:/Users/niteris/dev/running-process/Dockerfile.linux-build)
- use Docker layer cache for Rust dependency warmup and the real wheel build
- `docker run` that built image with only `dist-dev/` mounted
- copy `/dist/*.whl` out to `dist-dev/`

The wheel is written to `dist-dev/` on the host.

## Pytest Image

The pytest image is also stable and contains only test tools:

- `python:3.11-alpine`
- `bash`
- `pytest`
- `pytest-timeout`

It does not copy the repo or the wheel into the image.

At runtime the script:

- bind-mounts the repo at `/work`
- bind-mounts `dist-dev/` at `/dist-dev`
- copies `dist-dev/*.whl` into `/tmp/dist`
- installs that wheel
- runs pytest through `running_process.cli`

## Why This Is Fast

This avoids the main rebuild traps:

- changing pytest args does not rebuild any image
- changing tests does not rebuild any image
- changing source does not rebuild any image
- the only expensive repeated work is the actual wheel build, which reuses Cargo caches

The pytest image is long-lived.

For the builder:

- source enters through selective `COPY`
- Docker build cache handles dependency reuse
- only the output directory is runtime-mounted

## Cache Layout

Named Docker volumes:

- `running-process-alpine-pytest-pip`

Host directory:

- `dist-dev/`

## Commands

Build the Alpine musl wheel into `dist-dev/`:

```bash
python -m ci.linux_build_wheel
```

Run pytest against the wheel in `dist-dev/`:

```bash
python -m ci.linux_pytest
```

Run both in order:

```bash
python -m ci.linux_docker all
```

Focused pytest example:

```bash
python -m ci.linux_pytest --pytest-args "tests/test_pty_support.py -k chain_next_expect -ra"
```

Explicit platform example:

```bash
python -m ci.linux_build_wheel --platform linux/amd64
python -m ci.linux_pytest --platform linux/amd64
```

## Windows Host Note

Do not run `C:\Users\niteris\dev\running-process\.venv\bin\python` directly on Windows.

Use:

- `python -m ci.linux_build_wheel`
- `python -m ci.linux_pytest`
- `python -m ci.linux_docker all`

or the equivalent `uv run --module ...` forms if you want repo-managed Python execution.

## One Technical Assumption

The current design assumes the Alpine builder can successfully produce a wheel with:

```bash
--compatibility musllinux_1_2
```

That is the intended path and the right first attempt for a pure Alpine build pipeline.

If that proves false in practice, the architecture still stays the same:

- build image
- `dist-dev/*.whl`
- pytest image

Only the builder base image would need to change.

## Final Recommendation

Keep the contract exactly this small:

1. build wheel into `dist-dev/`
2. install wheel from `dist-dev/`
3. run pytest in Alpine

That is much cleaner than the previous all-in-one Dockerfile approach.
