# Changelog

This project follows [Semantic Versioning](https://semver.org/).

## Unreleased

- Report `host.id` and `aws.ecs.container_instance.tag.*` on ECS Anywhere, from
  the Systems Manager managed instance the task runs on. The lookup takes the
  credentials the environment supplies and three permissions on the task role,
  which `iam/detector-policy.json` grants; a permission the role lacks costs the
  attributes behind it and leaves a notice on standard error.

## 0.1.0

- Detect the cloud, container, task, and log attributes of an Amazon ECS task,
  from the task metadata endpoint and the local cgroup.
