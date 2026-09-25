/* Explicit synthetic lifecycle source, not logind evidence. Real process/pipe faults. */
#define _GNU_SOURCE
#include <stdint.h>
#include <unistd.h>
#include <time.h>
#include <signal.h>
#include <string.h>
#include <stdlib.h>
static void readall(unsigned char *b, size_t n) {
  while(n) { ssize_t k=read(0,b,n); if(k<=0) exit(0); b+=k; n-=(size_t)k; }
}
static uint64_t now(void) { struct timespec t; if(clock_gettime(CLOCK_BOOTTIME,&t)) exit(5); return (uint64_t)t.tv_sec*1000000000+(uint64_t)t.tv_nsec; }
int main(void) {
 unsigned char selection[224], q[32], r[40]; readall(selection,sizeof(selection));
 char mode[65]={0}; memcpy(mode,selection+32,selection[28]);
 if(!strcmp(mode,"hang")) raise(SIGSTOP);
 uint64_t frozen=now()+220000000; int count=0;
 for(;;) {
   readall(q,sizeof(q)); memset(r,0,sizeof(r)); memcpy(r,q,28); memcpy(r,"FRMR",4);
   uint64_t until=now()+300000000; unsigned code=1;
   if(!strcmp(mode,"frozen")) until=frozen;
   if(!strcmp(mode,"stale")) until=now()-1;
   if(!strcmp(mode,"opening")) {until=0; code=0;}
   if(!strcmp(mode,"locked") && count>=3) {until=0;code=2;}
   for(int i=0;i<8;i++) r[28+i]=(unsigned char)(until>>(56-8*i));
   r[37]=(unsigned char)code;
   if(!strcmp(mode,"wrong")) r[19]^=1;
   if(!strcmp(mode,"sequence")) r[27]^=1;
   if(!strcmp(mode,"split")) {
     for(int i=0;i<40;i++) {if(write(1,r+i,1)!=1) return 0; usleep(500);}
   } else if(write(1,r,sizeof(r))!=sizeof(r)) return 0;
   if(!strcmp(mode,"exit")) return 0;
   if(!strcmp(mode,"extra") && write(1,r,sizeof(r))!=sizeof(r)) return 0;
   count++;
 }
}
