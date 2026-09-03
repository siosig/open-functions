FROM python:3.14-slim
COPY --from=ghcr.io/astral-sh/uv:latest /uv /bin/
WORKDIR /app
COPY requirements.txt .
RUN uv pip install --system --compile-bytecode -r requirements.txt
COPY . .
ENV FUNCTION_TARGET=on_msg FUNCTION_SIGNATURE_TYPE=cloudevent PORT=8080 LOG_EXECUTION_ID=true PYTHONUNBUFFERED=1
CMD ["functions-framework"]
