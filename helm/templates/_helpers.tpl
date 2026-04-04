{{- define "fumen.name" -}}
fumen
{{- end -}}

{{- define "fumen.fullname" -}}
{{- printf "%s-%s" .Release.Name (include "fumen.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "fumen.backend.fullname" -}}
{{- printf "%s-backend" (include "fumen.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "fumen.frontend.fullname" -}}
{{- printf "%s-frontend" (include "fumen.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "fumen.baseDomain" -}}
{{- $parts := splitList "." .Values.frontend.ingress.domain -}}
{{- if gt (len $parts) 2 -}}
{{- join "." (slice $parts 1 (len $parts)) -}}
{{- else -}}
{{- .Values.frontend.ingress.domain -}}
{{- end -}}
{{- end -}}

{{- define "fumen.certificateName" -}}
{{- printf "%s-cert" (include "fumen.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "fumen.certificateSecretName" -}}
{{- printf "%s-cert-tls" (include "fumen.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "fumen.issuerName" -}}
{{- printf "%s-cert-issuer" (include "fumen.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "fumen.issuerAccountSecretName" -}}
{{- printf "%s-cert-issuer-account-key" (include "fumen.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
