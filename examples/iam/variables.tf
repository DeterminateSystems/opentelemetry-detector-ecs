variable "cluster_arn" {
  description = "The ARN of the ECS cluster running the task. It confines both ECS calls."
  type        = string

  validation {
    condition     = can(regex("^arn:[^:]+:ecs:", var.cluster_arn))
    error_message = "The cluster_arn must be an ECS cluster ARN, not a cluster name."
  }
}

variable "role_name" {
  description = "The name of the task role to create."
  type        = string
  default     = "opentelemetry-detector-ecs"
}
