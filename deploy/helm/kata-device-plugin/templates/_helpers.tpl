{{- define "kata-device-plugin.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else if eq .Release.Name .Chart.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "kata-device-plugin.devicePluginPath" -}}
{{- if .Values.hostPaths.devicePlugin -}}
{{- .Values.hostPaths.devicePlugin -}}
{{- else if eq .Values.k8sDistribution "microk8s" -}}
/var/snap/microk8s/common/var/lib/kubelet/device-plugins
{{- else -}}
/var/lib/kubelet/device-plugins
{{- end -}}
{{- end -}}

{{- define "kata-device-plugin.cdiPath" -}}
{{- if .Values.hostPaths.cdi -}}
{{- .Values.hostPaths.cdi -}}
{{- else if eq .Values.k8sDistribution "microk8s" -}}
/var/snap/microk8s/common/run/cdi
{{- else -}}
/var/run/cdi
{{- end -}}
{{- end -}}
