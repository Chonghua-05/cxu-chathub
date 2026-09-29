# cxu-chathub Web 控制台中间层
# 需要 docker CLI 操作宿主容器（docker restart），因此挂载 docker.sock
FROM node:20-alpine

# docker restart 需要容器内有 docker CLI（sock 由 compose 挂载）
RUN apk add --no-cache docker-cli

WORKDIR /app
COPY package.json ./
RUN npm install --omit=dev --registry=https://registry.npmmirror.com
COPY server.js ./
COPY public ./public

# 安全：不以 root 运行（docker.sock 组 gid 通常为 0，保持可写 sock 的组访问）
USER node

EXPOSE 9090
CMD ["node", "server.js"]
