# Changelog

This project follows [Semantic Versioning](https://semver.org/).

## 0.3.0

- Detect once. The first detection keeps what it found in memory, and every
  detection after it, from whatever thread, costs no more than a clone. A
  program can therefore give the detector to as many providers as it has.
  `EcsMetadata::detect_uncached` reads it all again for a program that expects
  the answer to change.

- Offer the detected task as `EcsMetadata`, a structure of fields rather than a
  list of attributes, which serializes and deserializes for whatever wants the
  description outside OpenTelemetry. `EcsMetadata::attributes` and
  `EcsMetadata::resource` turn it into what the detector reports.

- Leave out `aws.ecs.launchtype` when the metadata endpoint names no launch
  type, rather than report it empty.

## 0.2.0

- Offer a `fips` feature that serves the detector's AWS calls with
  FIPS-validated crypto, by building the SDK clients' TLS stack on
  `aws-lc-fips-sys`. Compiling it takes cmake, Go, and Perl.

- Put the ECS Anywhere lookup under an `anywhere` feature, on by default.
  `default-features = false` leaves the metadata-endpoint attributes and drops
  the AWS SDK dependencies, which are most of the dependency tree.

- Report `host.id` and `aws.ecs.container_instance.tag.*` on ECS Anywhere, from
  the Systems Manager managed instance the task runs on. The lookup takes the
  credentials the environment supplies and three permissions on the task role,
  which `iam/detector-policy.json` grants; a permission the role lacks costs the
  attributes behind it and leaves a notice on standard error.

## 0.1.0

- Detect the cloud, container, task, and log attributes of an Amazon ECS task,
  from the task metadata endpoint and the local cgroup.
