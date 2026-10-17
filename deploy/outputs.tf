output "host_ip" {
  description = "Public address of the mvd host."
  value       = google_compute_instance.host.network_interface[0].access_config[0].nat_ip
}

output "api_url" {
  description = "Base URL for mvctl (export MVD_URL=...)."
  value       = "http://${google_compute_instance.host.network_interface[0].access_config[0].nat_ip}:8080"
}
