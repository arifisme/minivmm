resource "google_compute_network" "mvd" {
  name                    = "${var.name}-network"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "mvd" {
  name          = "${var.name}-subnet"
  network       = google_compute_network.mvd.id
  region        = var.region
  ip_cidr_range = "10.42.0.0/24"
}

resource "google_compute_firewall" "api" {
  name          = "${var.name}-api"
  network       = google_compute_network.mvd.name
  direction     = "INGRESS"
  source_ranges = var.api_allowed_cidrs
  target_tags   = ["${var.name}-host"]

  allow {
    protocol = "tcp"
    ports    = ["8080"]
  }
}

resource "google_compute_firewall" "ssh" {
  count         = length(var.ssh_allowed_cidrs) > 0 ? 1 : 0
  name          = "${var.name}-ssh"
  network       = google_compute_network.mvd.name
  direction     = "INGRESS"
  source_ranges = var.ssh_allowed_cidrs
  target_tags   = ["${var.name}-host"]

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}

resource "google_service_account" "host" {
  account_id   = "${var.name}-host"
  display_name = "mvd host"
}

resource "google_compute_instance" "host" {
  name         = "${var.name}-host"
  machine_type = var.machine_type
  zone         = var.zone
  tags         = ["${var.name}-host"]

  advanced_machine_features {
    enable_nested_virtualization = true
  }

  boot_disk {
    initialize_params {
      image = "ubuntu-os-cloud/ubuntu-2404-lts-amd64"
      size  = var.disk_size_gb
      type  = "pd-balanced"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.mvd.id

    access_config {}
  }

  shielded_instance_config {
    enable_secure_boot = true
  }

  service_account {
    email  = google_service_account.host.email
    scopes = ["logging-write", "monitoring-write"]
  }

  metadata = {
    enable-oslogin = "TRUE"
    startup-script = templatefile("${path.module}/bootstrap.sh.tftpl", {
      repository  = var.repository
      git_ref     = var.git_ref
      api_token   = var.api_token
      mvd_service = file("${path.module}/mvd.service")
    })
  }
}
