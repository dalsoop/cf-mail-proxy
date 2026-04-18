FROM python:3.12-slim
WORKDIR /app
RUN pip install --no-cache-dir aiosmtpd requests
COPY proxy.py /app/proxy.py
EXPOSE 2525
CMD ["python", "/app/proxy.py"]
