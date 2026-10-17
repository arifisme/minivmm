variable "project" {
  description = "GCP project to deploy into."
  type        = string
}

variable "region" {
  description = "Region for the network."
  type        = string
  default     = "us-central1"
}

variable "zone" {
  description = "Zone for the VM. Nested virtualization needs an Intel or AMD machine type that supports it."
  type        = string
  default     = "us-central1-a"
}

variable "name" {
  description = "Prefix for every resource."
  type        = string
  default     = "mvd"
}

variable "machine_type" {
  description = "Machine type for the host. N2 and C3 types support nested virtualization."
  type        = string
  default     = "n2-standard-8"
}

variable "disk_size_gb" {
  description = "Boot disk size; function snapshots and build trees live on it."
  type        = number
  default     = 100
}

variable "api_allowed_cidrs" {
  description = "Networks that may reach the mvd API on port 8080."
  type        = list(string)
}

variable "ssh_allowed_cidrs" {
  description = "Networks that may reach SSH on port 22."
  type        = list(string)
  default     = []
}

variable "api_token" {
  description = "Bearer token mvd requires on every API request."
  type        = string
  sensitive   = true

  validation {
    condition     = length(var.api_token) >= 24
    error_message = "Use an API token of at least 24 characters."
  }
}

variable "repository" {
  description = "Git repository to build mvd from."
  type        = string
  default     = "https://github.com/arifisme/minivmm.git"
}

variable "git_ref" {
  description = "Branch, tag or commit to build."
  type        = string
  default     = "main"
}
