{{/* Chart-wide names and labels. */}}

{{- define "rhema-claw.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "rhema-claw.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "rhema-claw.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | quote }}
{{- end }}

{{- define "rhema-claw.labels" -}}
helm.sh/chart: {{ include "rhema-claw.chart" . }}
{{ include "rhema-claw.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "rhema-claw.selectorLabels" -}}
app.kubernetes.io/name: {{ include "rhema-claw.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "rhema-claw.imageTag" -}}
{{- default .Chart.AppVersion .Values.image.tag }}
{{- end }}

{{/*
The single-node invariant, enforced at render time: one daemon owns every
session in one SQLite store, with one dispatcher — scaling past one replica
would corrupt the store, not share the load (specs/rhema-framework-v1.md §7).
*/}}
{{- define "rhema-claw.assertSingleReplica" -}}
{{- if ne (int .Values.replicaCount) 1 }}
{{- fail (printf "replicaCount must be 1, got %v: rhema-claw is single-node by design (one SQLite store, one dispatcher; specs/rhema-framework-v1.md §7)" .Values.replicaCount) }}
{{- end }}
{{- end }}
