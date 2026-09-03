FROM python:3.14-slim
COPY --from=ghcr.io/astral-sh/uv:latest /uv /bin/
WORKDIR /app
COPY requirements.txt .
RUN uv pip install --system --compile-bytecode -r requirements.txt functions-framework==3.10.2
COPY . .
ENV FUNCTION_TARGET=hello FUNCTION_SIGNATURE_TYPE=http PORT=8080 LOG_EXECUTION_ID=true PYTHONUNBUFFERED=1
CMD ["functions-framework"]
