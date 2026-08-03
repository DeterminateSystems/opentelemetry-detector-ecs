terraform {
  required_version = ">= 1.6"

  required_providers {
    aws = {
      source = "hashicorp/aws"

      # Version 6 renamed the region attribute of the aws_region data source
      # from name to region, which main.tf uses.
      version = ">= 6.0"
    }
  }
}
