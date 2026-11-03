/* Integer, memcpy, syscall and fork timings; the same source runs natively
 * on macOS (clang) and in the probe guest (static musl). */
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/wait.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
int main(void){
  double t=now(); uint64_t x=88172645463325252ULL,s=0;
  for(long i=0;i<200000000L;i++){x^=x<<13;x^=x>>7;x^=x<<17;s+=x;}
  double ti=now()-t;
  size_t n=64<<20; char*a=malloc(n),*b=malloc(n); memset(a,1,n);
  t=now(); for(int i=0;i<20;i++){memcpy(b,a,n);a[i]=b[n-1-i];}
  double tm=now()-t;
  t=now(); for(int i=0;i<1000000;i++) s+=getppid();
  double ts=now()-t;
  t=now(); for(int i=0;i<500;i++){pid_t p=fork(); if(!p)_exit(0); waitpid(p,0,0);}
  double tf=now()-t;
  printf("PROBE: bench int=%.3f mem=%.3f sys=%.3f fork=%.3f chk=%llu\n",ti,tm,ts,tf,(unsigned long long)(s&0xff));
  return 0;
}
