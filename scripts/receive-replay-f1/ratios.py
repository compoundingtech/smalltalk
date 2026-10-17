"""UNRUN metadata-only reporting; the inspected mechanism stays byte-identical."""
LABELS = {'swap':'name-only-swap-including-commit-with-reader-open',
          'index':'populated-index-repair-including-commit-with-reader-open',
          'retire':'populated-retirement-including-commit'}
def report(cases):
    values={(c['pairs'],c['total_rows_per_side']):{p['label']:p.get('seconds_including_commit') for p in c['phases']} for c in cases}
    def ratio(a,b,ap,bp):
        x=values.get(a,{}).get(LABELS[ap]);y=values.get(b,{}).get(LABELS[bp])
        if x is None or y is None or x<0 or y<=0:return {'state':'unavailable','reason':'missing/capped/nonpositive denominator'}
        return {'state':'observed','ratio':x/y,'numerator_seconds':x,'denominator_seconds':y}
    return {'row_131072_over_16384':{p:ratio((1,131072),(1,16384),p,p) for p in ('index','retire')},
            'schema_fixed_131072':{str(n):ratio((n,131072),(1,131072),'swap','swap') for n in (8,24)},
            'within_case':{f'{n}/{r}':{p:ratio((n,r),(n,r),p,'swap') for p in ('index','retire')} for n,r in values},
            'scope':'disposable slope only, not production or native WriterGuard qualification'}
