# Project guide

This Rust application downloads selected NOAA GFS GRIB messages, writes global
and interpolated regional Zarr datasets to S3-compatible storage, and removes
expired datasets. Keep ingestion behavior separate from deployment tooling.

- Preserve `Cargo.lock` and use `cargo build --locked` / `cargo test --locked`.
  ecCodes and Blosc development libraries are required; the Dockerfile's builder
  stage provides them. The real-GRIB integration test is ignored unless its input
  is explicitly supplied.
- Never store passwords, access keys, private keys, kubeconfig, or Secret values
  in the repository, image, documentation, or logs. Kubernetes authentication is
  referenced through `default/rustfs-credentials`.
- `noaa-gfs-cronjob.yaml` declares the ConfigMap and CronJob in `default`.
  Keep the explanatory header and prerequisites current. Review server dry-run
  and diff before an authorized cluster change.
- The deployment intentionally uses `ghcr.io/uiui611/noaa-gfs-ingest:main` with
  `imagePullPolicy: Always`. Main pushes publish the image, then send an optional
  OIDC-authenticated webhook. Notification failures are warnings, with no retries
  or client-side payload validation.
- A webhook updates the CronJob's Pod template; subsequent scheduled Jobs use the
  new image. Do not create an immediate Job or interrupt a running Job as part of
  an image update. Manual ingestion changes stored data and requires explicit
  authorization.
