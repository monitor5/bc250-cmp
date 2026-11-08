# bc250mon

BC-250 두 대를 위한 초경량 웹 모니터. 노드마다 **정적 바이너리 1개(약 600 KB, RSS 약 1 MB)** 만 돌리면 되고, 같은 바이너리가 웹 UI도 제공합니다. 외부 의존성, DB, Python, Node, Docker 모두 없음.

## 기능
- 두 노드의 CPU / GPU 사용률 그래프 (15m, 1h, 6h, 24h, 7d), 구간 평균 + 구간 최대(점선)
- 기록: 2초 간격 최근 1시간(RAM), 1분 평균 N일(기본 7일, 디스크 `/var/lib/bc250mon/history.bin`, 분당 20 byte pwrite)
- 메모리: RAM, GTT(GPU가 시스템 RAM에서 가져간 양), VRAM carve-out, swap
- GPU 온도 / CPU(k10temp) 온도 / GPU 전력 / shader clock, 코어별 CPU
- 노드별 프로세스 목록: CPU%, **프로세스별 GPU% 와 VRAM/GTT** (amdgpu fdinfo), RSS, 스레드, 명령줄. 정렬, 필터, 커널 스레드 숨김

## 구조
```
브라우저 (VPN 내부) ──► bc250-1:9250  (UI + /api/*)
                    └─► bc250-2:9250  (/api/*, CORS 허용)
```
어느 노드로 접속해도 `PEERS`에 적은 두 노드를 모두 보여줍니다. 중앙 서버가 없어서 한 대가 죽어도 다른 한 대로 계속 볼 수 있습니다.

데이터 소스: `/proc/stat`, `/proc/meminfo`, `/proc/<pid>/{stat,cmdline,fd,fdinfo}`, `/sys/class/drm/cardN/device/{gpu_busy_percent,mem_info_*,pp_dpm_sclk,hwmon}`.
샘플링은 수집 스레드 1개가 2초마다(GPU는 0.5초마다 sub-sample) 파일 몇 개를 읽는 게 전부이고, 프로세스 스캔은 브라우저가 프로세스 목록을 볼 때만(5초마다, 탭이 보일 때만) 실행됩니다.

## 설치 (각 노드)
```sh
tar xzf bc250mon-*.tar.gz && cd bc250mon
sudo ./deploy/install.sh
sudo vi /etc/default/bc250mon   # LISTEN=<이 노드의 VPN IP>:9250, NAME, PEERS
sudo systemctl enable --now bc250mon
```
브라우저에서 `http://<VPN IP>:9250/` 접속.

systemd 유닛은 `MemoryMax=16M`, `CPUQuota=10%`, `Nice=10`, `IOSchedulingClass=idle` 로 묶여 있어서 추론 작업의 RAM/CPU를 절대 넘보지 못합니다. 권한은 root지만 capability를 `CAP_SYS_PTRACE`(다른 프로세스의 fd 읽기, 프로세스별 GPU 사용량용)와 `CAP_DAC_READ_SEARCH`로 제한하고, 파일시스템은 `ProtectSystem=strict`로 읽기 전용입니다.

**보안**: 인증이 없으므로 반드시 `LISTEN`을 VPN 인터페이스 IP로 지정하세요(0.0.0.0 금지). 읽기 전용 API라 프로세스를 죽이거나 설정을 바꾸는 기능은 없습니다.

## 옵션
```
--listen ADDR    기본 0.0.0.0:9250
--name NAME      기본 hostname
--peers LIST     name=http://ip:port,name2=http://ip2:port
--data FILE      기본 /var/lib/bc250mon/history.bin ('none' = RAM만)
--days N         1분 기록 보관 일수 (기본 7, 30일이어도 RAM ~0.9 MB)
--interval SEC   기본 2
--gpu-dev PATH   amdgpu sysfs 디렉터리 (기본 자동 탐지, vendor 0x1002)
```

## API
- `GET /api/now` 현재 값
- `GET /api/hist?sec=3600&n=600` 기록 (columnar JSON, 최대 n 버킷)
- `GET /api/procs?n=300` 프로세스 (0.5초 간격 2회 스냅샷으로 CPU%/GPU% 계산)
- `GET /api/info` 노드 이름, peers

## 빌드
```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo test --release
```
