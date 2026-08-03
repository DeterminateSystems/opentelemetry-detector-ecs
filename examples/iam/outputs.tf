output "role_arn" {
  description = "The task role. Give it to the task definition as its task_role_arn."
  value       = aws_iam_role.task.arn
}

output "role_name" {
  description = "The name of the task role, for attaching policies of your own."
  value       = aws_iam_role.task.name
}

output "policy_json" {
  description = "The policy alone, for a role you already have."
  value       = data.aws_iam_policy_document.detector.json
}
