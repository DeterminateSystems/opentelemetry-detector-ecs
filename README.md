# `opentelemetry-detector-ecs`

[![crates.io](https://img.shields.io/crates/v/opentelemetry-detector-ecs.svg)](https://crates.io/crates/opentelemetry-detector-ecs)
[![docs.rs](https://docs.rs/opentelemetry-detector-ecs/badge.svg)](https://docs.rs/opentelemetry-detector-ecs)

An [OpenTelemetry](https://opentelemetry.io/) resource detector for [Amazon ECS](https://aws.amazon.com/ecs/).

The detector reads the ECS task metadata endpoint and reports the cloud, container, task, and log attributes named by the [semantic conventions for ECS][conventions].
Anywhere else it reports nothing, so a program that also runs outside ECS can register it unconditionally.

## Usage

```rust
use opentelemetry_detector_ecs::EcsResourceDetector;
use opentelemetry_sdk::Resource;

let resource = Resource::builder()
    .with_detector(Box::new(EcsResourceDetector))
    .build();
```

Detection blocks for up to two seconds while it queries the metadata endpoint, and it reports whatever it has gathered so far if the endpoint answers slowly, partially, or not at all.

## Attributes

Every key below is a public constant in the crate's `attributes` module.
All but the last two come from [`opentelemetry-semantic-conventions`](https://docs.rs/opentelemetry-semantic-conventions), which names no key for a managed instance.

| Attribute                                                          | Source                                      |
| ------------------------------------------------------------------ | ------------------------------------------- |
| `cloud.provider`, `cloud.platform`                                 | Constant                                    |
| `container.name`                                                   | `$HOSTNAME`, or `/proc/sys/kernel/hostname` |
| `container.id`                                                     | `/proc/self/cgroup`                         |
| `cloud.region`, `cloud.account.id`                                 | The task ARN                                |
| `cloud.availability_zone`                                          | The task metadata                           |
| `aws.ecs.cluster.arn`, `aws.ecs.launchtype`                        | The task metadata                           |
| `aws.ecs.task.arn`, `aws.ecs.task.family`, `aws.ecs.task.revision` | The task metadata                           |
| `cloud.resource_id`, `aws.ecs.container.arn`                       | The container metadata                      |
| `aws.log.group.*`, `aws.log.stream.*`                              | The `awslogs` log driver options            |
| `aws.ecs.container_instance.arn`, `host.id`                        | ECS, on ECS Anywhere                        |
| `aws.ssm.managed_instance.tag.*`                                   | Systems Manager, on ECS Anywhere            |

Rows four through nine need the v4 metadata endpoint.
A task on the v3 endpoint gets the container name and ID alone.

## ECS Anywhere

A task with the `EXTERNAL` launch type runs on hardware of your own that the ECS agent registered as a Systems Manager managed instance.
The instance carries an `mi-` ID and whatever tags you gave it, and neither reaches the task metadata endpoint, so the detector goes to the APIs for them:

1. `ecs:DescribeTasks` names the container instance holding the task, and the service owning it.
2. `ecs:DescribeContainerInstances` turns that container instance into an `mi-` managed instance ID, which lands in `host.id`.
3. `ssm:ListTagsForResource` lists the tags on that managed instance, which land under `aws.ssm.managed_instance.tag.`, one attribute apiece.

The detector takes the credentials the task role supplies and asks for nothing else.
It prints a notice on standard error for each thing it cannot learn — absent credentials, a missing permission, a task no ECS service owns — and reports whatever it did learn.

This lookup runs only on the `EXTERNAL` launch type, and it adds up to five seconds to detection.

### Permissions

Grant the three permissions to the **task role**, the role a task definition names as `taskRoleArn`.
The execution role is the one ECS uses to pull the image and write the logs; the detector runs inside the container, so what it gets is the task role.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "DescribeTheTaskAndItsContainerInstance",
      "Effect": "Allow",
      "Action": ["ecs:DescribeTasks", "ecs:DescribeContainerInstances"],
      "Resource": "*",
      "Condition": {
        "ArnEquals": {
          "ecs:cluster": "arn:aws:ecs:REGION:ACCOUNT:cluster/CLUSTER"
        }
      }
    },
    {
      "Sid": "ListTheTagsOfAnyManagedInstance",
      "Effect": "Allow",
      "Action": "ssm:ListTagsForResource",
      "Resource": "arn:aws:ssm:REGION:ACCOUNT:managed-instance/mi-*"
    }
  ]
}
```

Neither ECS action takes a resource of its own, so the `ecs:cluster` condition is what confines them to one cluster.
The Systems Manager action does take one, and `managed-instance/mi-*` reaches every node the ECS agent registered from outside AWS and nothing else in Systems Manager.

Grant fewer than three and the detector reports what it can:
`ecs:DescribeTasks` alone yields nothing, since it only names the container instance to ask about next;
adding `ecs:DescribeContainerInstances` yields `aws.ecs.container_instance.arn` and `host.id`;
adding `ssm:ListTagsForResource` yields the tags.
A missing permission costs attributes, never startup.

[examples/iam](./examples/iam) is a Terraform module that builds the role and the policy.

## Development

```console
$ nix develop
$ just
```

The flake supplies the tools and the [Justfile](./Justfile) decides what to run with them, so `just ci` runs exactly what CI runs: the tests, Clippy, rustdoc, the formatting and spelling checks, and a packaging dry run.
Run `just` alone to list the recipes, or `just fmt` to format the tree in place.

## Releasing

Raise the version in `Cargo.toml`, move the `Unreleased` heading in [CHANGELOG.md](./CHANGELOG.md) down to it, and tag the merged commit `v<version>`.
Pushing the tag publishes the crate to crates.io and opens a GitHub release.
The workflow refuses a tag that disagrees with the manifest.

## License

Apache 2.0. See [LICENSE](./LICENSE).

The detector is a port of the [ECS detector in `opentelemetry-go-contrib`][go-contrib], likewise Apache 2.0.

[conventions]: https://opentelemetry.io/docs/specs/semconv/resource/cloud-provider/aws/ecs/
[go-contrib]: https://github.com/open-telemetry/opentelemetry-go-contrib/tree/main/detectors/aws/ecs
