# IAM for ECS Anywhere

A Terraform module that builds the task role `opentelemetry-detector-ecs` needs to describe the managed instance behind a task on ECS Anywhere.

## Usage

```hcl
module "detector_iam" {
  source = "github.com/DeterminateSystems/opentelemetry-detector-ecs//examples/iam"

  cluster_arn = aws_ecs_cluster.anywhere.arn
}

resource "aws_ecs_task_definition" "app" {
  family                = "app"
  requires_compatibilities = ["EXTERNAL"]
  task_role_arn         = module.detector_iam.role_arn

  # ...
}
```

The role belongs on the task definition as `task_role_arn`, not `execution_role_arn`.
The execution role is what ECS itself uses to pull the image and write the logs; the task role is what the process inside the container gets, and the detector runs inside the container.

## Attaching to a role you already have

The module publishes the policy on its own:

```hcl
resource "aws_iam_role_policy" "detector" {
  name   = "opentelemetry-detector-ecs"
  role   = aws_iam_role.existing.id
  policy = module.detector_iam.policy_json
}
```

## What each permission buys

| Permission                       | Attributes                                      |
| -------------------------------- | ----------------------------------------------- |
| `ecs:DescribeTasks`              | None directly; it names the container instance  |
| `ecs:DescribeContainerInstances` | `aws.ecs.container_instance.arn`, `host.id`     |
| `ssm:ListTagsForResource`        | `aws.ssm.managed_instance.tag.*`                |

The detector walks the three in order and stops where the permissions stop, so a role short of the last one still reports `host.id`.
Whatever it cannot learn becomes a notice on standard error rather than an error, and detection carries on.

## Scope

Both ECS actions take no resource of their own, so the `ecs:cluster` condition confines them to the one cluster.
The Systems Manager action takes the `managed-instance/mi-*` ARN, which covers every node the ECS agent registered from outside AWS and nothing else in Systems Manager.

Narrow the tag permission further with a tag condition if you want the role to read some instances and not others:

```hcl
condition {
  test     = "StringEquals"
  variable = "aws:ResourceTag/Environment"
  values   = ["production"]
}
```
