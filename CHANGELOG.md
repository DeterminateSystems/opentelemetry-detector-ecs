# Changelog

This project follows [Semantic Versioning](https://semver.org/).

## Unreleased

- On ECS Anywhere, report the `mi-` managed instance ID of the container
  instance running the task, its ARN, and the tags Systems Manager holds on it.
  The detector takes the credentials the environment supplies and prints a
  notice on standard error for whatever it cannot learn.
- Document the three permissions that lookup needs, and add a Terraform module
  granting them, under `examples/iam`.

## 0.1.0

- Detect the cloud, container, task, and log attributes of an Amazon ECS task,
  from the task metadata endpoint and the local cgroup.
