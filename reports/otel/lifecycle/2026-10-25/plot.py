"""Regenerate memory.svg/png from archived snapshots; requires matplotlib."""
import json
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
root=Path(__file__).resolve().parent
fig, axes=plt.subplots(2,1,figsize=(10,7),sharex=True,layout='constrained')
for ax,arch,title in zip(axes,('x86_64','aarch64'),('Linux x86-64 · GStreamer 1.24.2','Linux arm64 · GStreamer 1.22.0 (container)')):
 values=json.loads((root/arch/'snapshots.json').read_text())
 assert values[-1]['phase']=='shutdown'
 times=[v['elapsed_ms']/60000 for v in values]
 ax.plot(times,[v['rss_kb']/1024 for v in values],label='Observed RSS',color='#2563a6',linewidth=1.7)
 ax.plot(times,[v['peak_kb']/1024 for v in values],label='Observed high-water mark',color='#c87821',linewidth=1,linestyle='--')
 ax.axvspan(0,5,color='#e5e7eb',alpha=.6,label='First five minutes')
 end=values[-1]
 ax.scatter([times[-1]],[end['rss_kb']/1024],color='#2563a6',s=25,zorder=4)
 ax.annotate('After shutdown',xy=(times[-1],end['rss_kb']/1024),xytext=(-110,-15),textcoords='offset points',arrowprops={'arrowstyle':'->','color':'#536174'},fontsize=9)
 ax.set_title(title,loc='left',fontsize=11)
 ax.set_ylabel('Process RSS (MiB)')
 ax.set_ylim(bottom=0)
 ax.grid(axis='y',color='#d9dee5',linewidth=.5)
 ax.spines[['top','right']].set_visible(False)
axes[0].legend(loc='lower left',fontsize=9,ncols=3,frameon=False)
axes[-1].set_xlabel('Elapsed time (minutes)')
axes[-1].set_xlim(0,31)
fig.suptitle('Thirty-minute lifecycle soaks with collector outages and library loading',fontsize=13)
fig.savefig(root/'memory.svg')
fig.savefig(root/'memory.png',dpi=160)
