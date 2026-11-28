import unittest, math, copy
from cohort_metrics import aggregate_calls

class CohortTests(unittest.TestCase):
    def test_failures_missing_and_invalid_values_stay_distinct(self):
        rows=[{'protocol_complete':True,'first_content_s':1,'total_s':4}, {'protocol_complete':True,'first_content_s':3.,'total_s':6}, {'protocol_complete':False,'first_content_s':0,'total_s':.1}, {'protocol_complete':1,'total_s':.2}, {'protocol_complete':True,'first_content_s':True,'total_s':float('inf')}, {'protocol_complete':True,'first_content_s':float('nan'),'tail_wait_s':-1}, {'protocol_complete':True,'first_content_s':'2'}, {'protocol_complete':True,'first_content_s':-5}]
        original=copy.deepcopy(rows);result=aggregate_calls(rows)
        self.assertEqual(result['attempts'],8);self.assertEqual(result['protocol_complete'],6);self.assertEqual(result['protocol_incomplete'],2);self.assertEqual(result['complete_total_under5'],1)
        self.assertEqual(result['timings']['first_content_s'],{'n':2,'min':1,'median':2.,'max':3.})
        self.assertEqual(result['timings']['total_s'],{'n':2,'min':4,'median':5.,'max':6})
        self.assertEqual(result['timings']['tail_wait_s'],{'n':0,'min':None,'median':None,'max':None})
        self.assertEqual(rows[0],original[0]);self.assertTrue(math.isnan(rows[5]['first_content_s']))
    def test_empty_and_threshold_boundary(self):
        self.assertEqual(aggregate_calls([])['timings']['total_s']['n'],0)
        rows=[{'protocol_complete':True,'total_s':5}, {'protocol_complete':True,'total_s':4.999}]
        self.assertEqual(aggregate_calls(rows)['complete_total_under5'],1)
if __name__=='__main__':unittest.main()
