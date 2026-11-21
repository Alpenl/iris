#!/usr/bin/env python3
"""Lossless WebP ICC fixtures; uses Pillow rather than FFmpeg's optional encoder."""
from pathlib import Path
from PIL import Image, features

ROOT=Path(__file__).resolve().parents[1]
PROFILES=Path('/System/Library/ColorSync/Profiles')
OUT=ROOT/'test-media/photo/icc'
if not features.check('webp'): raise SystemExit('Pillow was built without WebP')
OUT.mkdir(parents=True,exist_ok=True)
for tag,profile in [('srgb','sRGB Profile.icc'),('p3','Display P3.icc'),('adobergb','AdobeRGB1998.icc')]:
    icc=(PROFILES/profile).read_bytes()
    for name,color in [('red',(255,0,0)),('green',(0,255,0)),('orange',(200,120,60))]:
        Image.new('RGB',(600,400),color).save(OUT/f'{name}-{tag}-webp.webp',lossless=True,icc_profile=icc)
print(f'Created 9 lossless WebP fixtures in {OUT}')
