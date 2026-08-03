# The permissions opentelemetry-detector-ecs needs on ECS Anywhere, and a task
# role carrying them.
#
# The detector makes three calls, and each one earns an attribute or two:
#
#   ecs:DescribeTasks               the container instance holding the task
#   ecs:DescribeContainerInstances  the mi- managed instance ID of that instance
#   ssm:ListTagsForResource         the tags on that managed instance
#
# Grant all three to see the whole set. Grant fewer and the detector reports
# what it can and prints a notice on standard error for the rest, so a partial
# grant costs attributes rather than startup.

data "aws_partition" "current" {}

data "aws_region" "current" {}

data "aws_caller_identity" "current" {}

# ECS assumes the task role on the task's behalf. The source conditions keep
# anyone else from talking ECS into assuming it for a task of their own.
data "aws_iam_policy_document" "assume_role" {
  statement {
    sid     = "LetEcsAssumeTheRoleForTasksOfThisAccount"
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:SourceAccount"
      values   = [data.aws_caller_identity.current.account_id]
    }

    condition {
      test     = "ArnLike"
      variable = "aws:SourceArn"
      values   = ["arn:${data.aws_partition.current.partition}:ecs:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:*"]
    }
  }
}

data "aws_iam_policy_document" "detector" {
  # Neither ECS call takes a resource, so the cluster condition is what
  # confines them. A task role scoped this way reads its own cluster and no
  # other.
  statement {
    sid = "DescribeTheTaskAndItsContainerInstance"

    actions = [
      "ecs:DescribeTasks",
      "ecs:DescribeContainerInstances",
    ]

    resources = ["*"]

    condition {
      test     = "ArnEquals"
      variable = "ecs:cluster"
      values   = [var.cluster_arn]
    }
  }

  # Systems Manager holds the tags. Only a node registered from outside AWS
  # carries an mi- ID, so the wildcard reaches no EC2 instance and no document,
  # parameter, or patch baseline.
  statement {
    sid     = "ListTheTagsOfAnyManagedInstance"
    actions = ["ssm:ListTagsForResource"]

    resources = [
      "arn:${data.aws_partition.current.partition}:ssm:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:managed-instance/mi-*",
    ]
  }
}

resource "aws_iam_role" "task" {
  name               = var.role_name
  description        = "Reads the ECS Anywhere managed instance behind a task, for opentelemetry-detector-ecs"
  assume_role_policy = data.aws_iam_policy_document.assume_role.json
}

resource "aws_iam_role_policy" "detector" {
  name   = "opentelemetry-detector-ecs"
  role   = aws_iam_role.task.id
  policy = data.aws_iam_policy_document.detector.json
}
