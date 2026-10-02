FROM python:3.13-slim
ENV PYTHONUNBUFFERED=1
WORKDIR /app
COPY main.py .
USER nobody
# Cloud Run ignores Docker health checks; it probes the port itself
# (configure HTTP probes with `service.health_check` in runway.yaml).
HEALTHCHECK NONE
CMD ["python", "main.py"]
